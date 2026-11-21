//! zustc's wasm32-unknown-unknown backend.
//!
//! - [`compile`] has the same shape as the Cranelift backend (zust-clif): it reads the instructions minclif's
//!   FunctionBuilder recorded (in the order acg made them) and returns one function's "code" with relocations. Here
//!   the code is a wasm function body, so caching, the fork-parallel compile, ICF and the object layout of
//!   driver2/build.rs work unchanged.
//! - [`write_module`] takes the functions and data that build.rs would have written into an ELF object and writes
//!   one wasm module instead (zustc compiles whole programs, so no wasm linker is needed).
//!
//! The memory model, and what keeps a program inside its sandbox:
//!
//! - Values keep the types of the IR: pointers and `usize` stay 64-bit (`i64`), and are narrowed to a 32-bit
//!   address at each memory access. Layouts are the same as on x86-64 (so `size_of::<usize>()` is 8, unlike
//!   rustc's wasm32).
//! - Linear memory is laid out stack first: a null page `[0, NULL_PAGE)`, the shadow stack above it growing down
//!   towards it, then the data, then the heap. A stack overflow takes the stack pointer into the null page, which
//!   every function entry checks (`unreachable`), so the stack can never run into the data.
//! - Every load and store checks its address: one that does not fit in 32 bits (narrowed, it would wrap onto other
//!   memory) or falls in the null page traps.
//! - Memory grows to at most `MAX_PAGES` (2 GiB): an address with the top bit set is always out of bounds, so a
//!   wrapped negative offset traps instead of reaching other data.
//! - Every SSA value is a wasm local and every block a case of one `loop` + `br_table` dispatcher (simple, and the
//!   IR's uses of values outside the blocks that define them read a local, as they read a slot on minclif).
//! - The allocator (`malloc` / `free` / `realloc` / `calloc`) and `memcpy` / `memmove` / `memset` are inside the
//!   module. Everything else the program calls is an import from module `env`: the host decides what to give it
//!   (tests/wasm/run.mjs gives `write` to fd 1 and 2, `exit`, the program's arguments and `Math`).

use minclif::{FloatCC, FunctionBuilder, IntCC, Op, Reloc, StackSlot, Type, Value};

/// Relocation kinds in a body (the ELF numbers the Blob keeps: 4 = PLT32, 1 = 64, 2 = PC32)
/// `call` of a function: a 5-byte padded LEB function index; the addend is the callee's signature ([`sig_enc`])
pub const R_CALL: u32 = 4;
/// `i64.const` of a symbol: a 10-byte padded signed LEB, the data's address (+ addend) or the function's table index
pub const R_ADDR: u32 = 1;
/// `call_indirect`'s type: a 5-byte padded LEB type index; the addend is the signature
pub const R_TYPE: u32 = 2;

/// The null page: no access below this address (a trap, as a null dereference faults natively)
pub const NULL_PAGE: u64 = 4096;
/// The shadow stack: [NULL_PAGE, NULL_PAGE + STACK_SIZE)
pub const STACK_SIZE: u64 = 1 << 20;
/// Memory never grows past this (2 GiB): addresses with the top bit set always trap
pub const MAX_PAGES: u64 = 32768;
/// The largest block `malloc` hands out (larger requests return null: the prelude then aborts)
const MAX_ALLOC: i64 = 1 << 30;

// wasm value types
const T_I32: u8 = 0x7F;
const T_I64: u8 = 0x7E;
const T_F32: u8 = 0x7D;
const T_F64: u8 = 0x7C;

/// Globals the module writer defines: the stack pointer and the top of the heap
const G_SP: u32 = 0;
const G_HEAP: u32 = 1;

fn wtys(t: Type) -> &'static [u8] {
    match t {
        Type::I8 | Type::I16 | Type::I32 => &[T_I32],
        Type::I64 => &[T_I64],
        Type::I128 => &[T_I64, T_I64],
        Type::F32 => &[T_F32],
        Type::F64 => &[T_F64],
    }
}

fn narrow_bits(t: Type) -> Option<u32> {
    match t {
        Type::I8 => Some(8),
        Type::I16 => Some(16),
        _ => None,
    }
}

// --- encodings -------------------------------------------------------------------------------------------

fn uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// A padded unsigned LEB of exactly 5 bytes (patched by the module writer)
fn uleb5_at(out: &mut [u8], v: u32) {
    let mut v = v;
    for i in 0..5 {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        out[i] = if i < 4 { b | 0x80 } else { b };
    }
}

/// A padded signed LEB of exactly 10 bytes
fn sleb10_at(out: &mut [u8], v: i64) {
    let mut v = v;
    for i in 0..10 {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        out[i] = if i < 9 { b | 0x80 } else { b & 0x7F };
    }
}

fn tcode(t: u8) -> i64 {
    match t {
        T_I32 => 0,
        T_I64 => 1,
        T_F32 => 2,
        _ => 3,
    }
}

fn tdec(c: i64) -> u8 {
    [T_I32, T_I64, T_F32, T_F64][(c & 3) as usize]
}

/// A wasm signature in an i64 (the relocation's addend): 2 bits of result count, 3 results of 2 bits, 5 bits of
/// parameter count, 25 parameters of 2 bits
pub fn sig_enc(params: &[u8], rets: &[u8]) -> Result<i64, String> {
    if rets.len() > 3 || params.len() > 25 {
        return Err(format!("wasm32: a signature with {} parameters and {} results is not supported", params.len(), rets.len()));
    }
    let mut e = rets.len() as i64;
    for (i, &r) in rets.iter().enumerate() {
        e |= tcode(r) << (2 + 2 * i);
    }
    e |= (params.len() as i64) << 8;
    for (i, &p) in params.iter().enumerate() {
        e |= tcode(p) << (13 + 2 * i);
    }
    Ok(e)
}

pub fn sig_dec(e: i64) -> (Vec<u8>, Vec<u8>) {
    let nr = (e & 3) as usize;
    let rets = (0..nr).map(|i| tdec(e >> (2 + 2 * i))).collect();
    let np = ((e >> 8) & 31) as usize;
    let params = (0..np).map(|i| tdec(e >> (13 + 2 * i))).collect();
    (params, rets)
}

// --- function bodies --------------------------------------------------------------------------------------

/// One function body being written
struct Body<'a> {
    fb: &'a FunctionBuilder,
    code: Vec<u8>,
    relocs: Vec<Reloc>,
    /// types of the locals after the parameters
    locals: Vec<u8>,
    n_param_locals: u32,
    /// value id → its first local
    vloc: Vec<u32>,
    /// variable id → (first local, type)
    var: Vec<(u32, Type)>,
    /// slot id → offset in the frame
    slot: Vec<u32>,
    slot_size: Vec<u32>,
    /// function ref id → name
    fname: Vec<String>,
    /// block id → case number
    case: Vec<u32>,
    n_cases: u32,
    /// the case being written, and how many `if`s are open in it
    cur_case: u32,
    nest: u32,
    l_label: u32,
    /// inside the dispatcher (a jump from the code before the first block only sets the label)
    in_loop: bool,
    l_fp: u32,
    l_saved_sp: u32,
    frame: u32,
    /// scratch locals by type (reused across ops: an op's scratch use never outlives it)
    scratch_i64: Vec<u32>,
    scratch_i32: Vec<u32>,
    /// check each address (see addr; ZUSTC_WASM_NO_ADDR_CHECK=1 turns it off, to measure what it costs)
    check_addr: bool,
}

impl<'a> Body<'a> {
    fn op(&mut self, b: u8) {
        self.code.push(b);
    }
    fn ops(&mut self, bs: &[u8]) {
        self.code.extend_from_slice(bs);
    }
    fn new_local(&mut self, t: u8) -> u32 {
        self.locals.push(t);
        self.n_param_locals + self.locals.len() as u32 - 1
    }
    fn ty(&self, v: Value) -> Type {
        self.fb.value_type(v)
    }
    fn loc(&self, v: Value) -> Result<u32, String> {
        match self.vloc.get(v.0 as usize) {
            Some(&l) if l != u32::MAX => Ok(l),
            _ => Err(format!("wasm32: unknown value v{}", v.0)),
        }
    }
    fn lget(&mut self, l: u32) {
        self.op(0x20);
        uleb(&mut self.code, l as u64);
    }
    fn lset(&mut self, l: u32) {
        self.op(0x21);
        uleb(&mut self.code, l as u64);
    }
    fn ltee(&mut self, l: u32) {
        self.op(0x22);
        uleb(&mut self.code, l as u64);
    }
    /// pushes v (both halves of an i128: low, then high)
    fn get(&mut self, v: Value) -> Result<(), String> {
        let l = self.loc(v)?;
        self.lget(l);
        if self.ty(v) == Type::I128 {
            self.lget(l + 1);
        }
        Ok(())
    }
    /// pops into the result local(s) of value id r
    fn set_id(&mut self, r: u32) -> Result<(), String> {
        let v = Value(r);
        let l = self.loc(v)?;
        if self.ty(v) == Type::I128 {
            self.lset(l + 1);
        }
        self.lset(l);
        Ok(())
    }
    fn i32c(&mut self, x: i32) {
        self.op(0x41);
        sleb(&mut self.code, x as i64);
    }
    fn i64c(&mut self, x: i64) {
        self.op(0x42);
        sleb(&mut self.code, x);
    }
    fn gget(&mut self, g: u32) {
        self.op(0x23);
        uleb(&mut self.code, g as u64);
    }
    fn gset(&mut self, g: u32) {
        self.op(0x24);
        uleb(&mut self.code, g as u64);
    }
    fn tmp64(&mut self, k: usize) -> u32 {
        while self.scratch_i64.len() <= k {
            let l = self.new_local(T_I64);
            self.scratch_i64.push(l);
        }
        self.scratch_i64[k]
    }
    fn tmp32(&mut self, k: usize) -> u32 {
        while self.scratch_i32.len() <= k {
            let l = self.new_local(T_I32);
            self.scratch_i32.push(l);
        }
        self.scratch_i32[k]
    }
    /// keeps the low bits of a narrow integer on the stack (narrow values are held zero-extended)
    fn norm(&mut self, t: Type) {
        if let Some(b) = narrow_bits(t) {
            self.i32c(((1u32 << b) - 1) as i32);
            self.op(0x71);
        }
    }
    /// sign-extends a narrow integer on the stack to i32
    fn sext32(&mut self, t: Type) {
        match t {
            Type::I8 => self.op(0xC0),
            Type::I16 => self.op(0xC1),
            _ => {}
        }
    }
    /// pushes v, sign-extended if narrow
    fn get_s(&mut self, v: Value) -> Result<(), String> {
        self.get(v)?;
        let t = self.ty(v);
        self.sext32(t);
        Ok(())
    }
    /// pushes v converted to the wasm type of t (a shift amount, which may be of another width)
    fn get_as(&mut self, v: Value, t: Type) -> Result<(), String> {
        self.get(v)?;
        let from = self.ty(v);
        let w64 = |t: Type| matches!(t, Type::I64 | Type::I128);
        match (w64(from), w64(t)) {
            (true, false) => {
                if from == Type::I128 {
                    self.op(0x1A); // (drop the high half)
                }
                self.op(0xA7);
            }
            (false, true) => self.op(0xAD),
            (true, true) if from == Type::I128 => self.op(0x1A),
            _ => {}
        }
        Ok(())
    }
    /// pushes the address v + off as an i32. Unless checks are off, an address that does not fit in 32 bits (it would
    /// wrap onto other memory) or is in the null page traps: one unsigned compare, (a - NULL_PAGE) > (2^32 - 1 - NULL_PAGE)
    fn addr(&mut self, v: Value, off: i32) -> Result<(), String> {
        self.get(v)?;
        if off != 0 {
            self.i64c(off as i64);
            self.op(0x7C);
        }
        if self.check_addr {
            let t = self.tmp64(8);
            self.ltee(t);
            self.i64c(NULL_PAGE as i64);
            self.op(0x7D);
            self.i64c(0xFFFF_FFFF - NULL_PAGE as i64);
            self.op(0x56);
            self.ops(&[0x04, 0x40, 0x00, 0x0B]);
            self.lget(t);
        }
        self.op(0xA7);
        Ok(())
    }
    /// Traps unless each pointer is a valid address (as addr checks one) or the count n is 0, and n fits in 32 bits
    fn check_range(&mut self, ptrs: &[Value], n: Value) -> Result<(), String> {
        let (t, c) = (self.tmp64(8), self.tmp64(10));
        self.get_as(n, Type::I64)?;
        self.ltee(c);
        self.i64c(0xFFFF_FFFF);
        self.op(0x56);
        self.ops(&[0x04, 0x40, 0x00, 0x0B]);
        self.lget(c);
        self.op(0x50);
        self.op(0x45);
        self.ops(&[0x04, 0x40]);
        for &p in ptrs {
            self.get(p)?;
            self.ltee(t);
            self.i64c(NULL_PAGE as i64);
            self.op(0x7D);
            self.i64c(0xFFFF_FFFF - NULL_PAGE as i64);
            self.op(0x56);
            self.ops(&[0x04, 0x40, 0x00, 0x0B]);
        }
        self.op(0x0B);
        Ok(())
    }
    /// memarg (alignment 1: always valid; offset 0)
    fn memarg(&mut self, off: u32) {
        self.op(0);
        uleb(&mut self.code, off as u64);
    }
    /// pushes the condition v as an i32 (non-zero is true)
    fn cond(&mut self, v: Value) -> Result<(), String> {
        self.get(v)?;
        match self.ty(v) {
            Type::I64 => {
                self.i64c(0);
                self.op(0x52);
            }
            Type::I128 => {
                self.op(0x84); // (or of the halves)
                self.i64c(0);
                self.op(0x52);
            }
            _ => {}
        }
        Ok(())
    }
    fn call_reloc(&mut self, name: &str, sig: i64) {
        self.op(0x10);
        let at = self.code.len();
        self.code.extend_from_slice(&[0; 5]);
        uleb5_at(&mut self.code[at..at + 5], 0);
        self.relocs.push(Reloc { offset: at as u32, name: name.to_string(), kind: R_CALL, addend: sig });
    }
    /// a jump to block b with args (the block's parameters are assigned all at once: pushed, then popped)
    fn jump(&mut self, b: u32, args: &[Value]) -> Result<(), String> {
        let params = self.fb.block_params(minclif::Block(b));
        if params.len() != args.len() {
            return Err(format!("wasm32: jump to block{b} with {} arguments for {} parameters", args.len(), params.len()));
        }
        for &a in args {
            self.get(a)?;
        }
        for &p in params.iter().rev() {
            self.set_id(p.0)?;
        }
        let c = *self.case.get(b as usize).filter(|&&c| c != u32::MAX).ok_or_else(|| format!("wasm32: jump to block{b}, which is never started"))?;
        self.i32c(c as i32);
        self.lset(self.l_label);
        if !self.in_loop {
            return Ok(());
        }
        // (to the dispatcher's loop: the cases after this one, and the open `if`s, are between)
        let depth = self.n_cases - 1 - self.cur_case + self.nest;
        self.op(0x0C);
        uleb(&mut self.code, depth as u64);
        Ok(())
    }
    fn ret(&mut self, vals: &[Value]) -> Result<(), String> {
        for &v in vals {
            self.get(v)?;
        }
        if self.frame > 0 {
            self.lget(self.l_saved_sp);
            self.gset(G_SP);
        }
        self.op(0x0F);
        Ok(())
    }
    fn trap_if(&mut self, c: Value, when_zero: bool) -> Result<(), String> {
        self.cond(c)?;
        if when_zero {
            self.op(0x45);
        }
        self.ops(&[0x04, 0x40, 0x00, 0x0B]);
        Ok(())
    }

    /// x * y's high 64 bits, unsigned (x, y in scratch i64 locals 0 and 1; uses 2..5); leaves it on the stack
    fn umulhi64(&mut self) {
        let (x, y) = (self.tmp64(0), self.tmp64(1));
        let (p01, p10, mid) = (self.tmp64(2), self.tmp64(3), self.tmp64(4));
        let m = 0xFFFF_FFFFi64;
        // p01 = lo(x) * hi(y); p10 = hi(x) * lo(y)
        self.lget(x);
        self.i64c(m);
        self.op(0x83);
        self.lget(y);
        self.i64c(32);
        self.op(0x88);
        self.op(0x7E);
        self.lset(p01);
        self.lget(x);
        self.i64c(32);
        self.op(0x88);
        self.lget(y);
        self.i64c(m);
        self.op(0x83);
        self.op(0x7E);
        self.lset(p10);
        // mid = (lo(x) * lo(y)) >> 32 + lo(p01) + lo(p10)
        self.lget(x);
        self.i64c(m);
        self.op(0x83);
        self.lget(y);
        self.i64c(m);
        self.op(0x83);
        self.op(0x7E);
        self.i64c(32);
        self.op(0x88);
        self.lget(p01);
        self.i64c(m);
        self.op(0x83);
        self.op(0x7C);
        self.lget(p10);
        self.i64c(m);
        self.op(0x83);
        self.op(0x7C);
        self.lset(mid);
        // hi(x) * hi(y) + p01 >> 32 + p10 >> 32 + mid >> 32
        self.lget(x);
        self.i64c(32);
        self.op(0x88);
        self.lget(y);
        self.i64c(32);
        self.op(0x88);
        self.op(0x7E);
        self.lget(p01);
        self.i64c(32);
        self.op(0x88);
        self.op(0x7C);
        self.lget(p10);
        self.i64c(32);
        self.op(0x88);
        self.op(0x7C);
        self.lget(mid);
        self.i64c(32);
        self.op(0x88);
        self.op(0x7C);
    }

    /// x * y's high 64 bits, signed (as umulhi64, minus y when x < 0 and x when y < 0)
    fn smulhi64(&mut self) {
        self.umulhi64();
        let (x, y) = (self.tmp64(0), self.tmp64(1));
        // - (x < 0 ? y : 0)
        self.lget(y);
        self.i64c(0);
        self.lget(x);
        self.i64c(0);
        self.op(0x53);
        self.op(0x1B);
        self.op(0x7D);
        self.lget(x);
        self.i64c(0);
        self.lget(y);
        self.i64c(0);
        self.op(0x53);
        self.op(0x1B);
        self.op(0x7D);
    }

    fn icmp_op(cc: IntCC, w64: bool) -> u8 {
        let base: u8 = if w64 { 0x51 } else { 0x46 };
        base + match cc {
            IntCC::Equal => 0,
            IntCC::NotEqual => 1,
            IntCC::SignedLessThan => 2,
            IntCC::UnsignedLessThan => 3,
            IntCC::SignedGreaterThan => 4,
            IntCC::UnsignedGreaterThan => 5,
            IntCC::SignedLessThanOrEqual => 6,
            IntCC::UnsignedLessThanOrEqual => 7,
            IntCC::SignedGreaterThanOrEqual => 8,
            IntCC::UnsignedGreaterThanOrEqual => 9,
        }
    }

    /// compares x and y (pushed by the caller as two values of type t)
    fn icmp(&mut self, cc: IntCC, x: Value, y: Option<(Value, i64)>) -> Result<(), String> {
        let t = self.ty(x);
        if t == Type::I128 {
            let Some((y, _)) = y else { return Err("wasm32: icmp_imm on i128".into()) };
            return self.icmp128(cc, x, y);
        }
        let signed = cc.signed();
        if signed { self.get_s(x)? } else { self.get(x)? }
        match y {
            Some((y, _)) => {
                if signed { self.get_s(y)? } else { self.get(y)? }
            }
            None => unreachable!(),
        }
        self.op(Self::icmp_op(cc, t == Type::I64));
        Ok(())
    }

    fn icmp_imm(&mut self, cc: IntCC, x: Value, k: i64) -> Result<(), String> {
        let t = self.ty(x);
        let signed = cc.signed();
        match t {
            Type::I64 => {
                self.get(x)?;
                self.i64c(k);
            }
            Type::I128 => return Err("wasm32: icmp_imm on i128".into()),
            _ => {
                if signed { self.get_s(x)? } else { self.get(x)? }
                let k = match narrow_bits(t) {
                    Some(b) if !signed => k & ((1i64 << b) - 1),
                    Some(b) => (k << (64 - b)) >> (64 - b),
                    None => k as i32 as i64,
                };
                self.i32c(k as i32);
            }
        }
        self.op(Self::icmp_op(cc, t == Type::I64));
        Ok(())
    }

    fn icmp128(&mut self, cc: IntCC, x: Value, y: Value) -> Result<(), String> {
        let (lx, ly) = (self.loc(x)?, self.loc(y)?);
        match cc {
            IntCC::Equal | IntCC::NotEqual => {
                self.lget(lx);
                self.lget(ly);
                self.op(0x51);
                self.lget(lx + 1);
                self.lget(ly + 1);
                self.op(0x51);
                self.op(0x71);
                if cc == IntCC::NotEqual {
                    self.op(0x45);
                }
            }
            _ => {
                // hi cc' hi (strict) || (hi == hi && lo cc_u lo)
                let strict = match cc {
                    IntCC::SignedLessThan | IntCC::SignedLessThanOrEqual => IntCC::SignedLessThan,
                    IntCC::SignedGreaterThan | IntCC::SignedGreaterThanOrEqual => IntCC::SignedGreaterThan,
                    IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual => IntCC::UnsignedLessThan,
                    _ => IntCC::UnsignedGreaterThan,
                };
                let lo = match cc {
                    IntCC::SignedLessThan | IntCC::UnsignedLessThan => IntCC::UnsignedLessThan,
                    IntCC::SignedLessThanOrEqual | IntCC::UnsignedLessThanOrEqual => IntCC::UnsignedLessThanOrEqual,
                    IntCC::SignedGreaterThan | IntCC::UnsignedGreaterThan => IntCC::UnsignedGreaterThan,
                    _ => IntCC::UnsignedGreaterThanOrEqual,
                };
                self.lget(lx + 1);
                self.lget(ly + 1);
                self.op(Self::icmp_op(strict, true));
                self.lget(lx + 1);
                self.lget(ly + 1);
                self.op(0x51);
                self.lget(lx);
                self.lget(ly);
                self.op(Self::icmp_op(lo, true));
                self.op(0x71);
                self.op(0x72);
            }
        }
        Ok(())
    }

    /// x op y for an integer type (narrow types computed in i32 and kept to their bits)
    fn binop(&mut self, x: Value, y: Value, op32: u8, op64: u8, signed: bool) -> Result<(), String> {
        let t = self.ty(x);
        if t == Type::I128 {
            return Err("wasm32: this i128 operation is not supported yet".into());
        }
        // (a pointer I64 plus an index I32 mixes widths: x86 extends implicitly, wasm needs matching types.
        //  compute in the wider width, zero-extending the narrow side)
        let wide = t == Type::I64 || self.ty(y) == Type::I64;
        if signed { self.get_s(x)? } else { self.get(x)? }
        if wide && t != Type::I64 { self.op(0xAD); }
        if signed { self.get_s(y)? } else { self.get(y)? }
        if wide && self.ty(y) != Type::I64 { self.op(0xAD); }
        self.op(if wide { op64 } else { op32 });
        if !wide { self.norm(t); }
        Ok(())
    }

    /// x << / >> y (the amount taken modulo the width, as Cranelift does)
    fn shift(&mut self, x: Value, y: Value, which: u8) -> Result<(), String> {
        let t = self.ty(x);
        match t {
            Type::I128 => self.shift128(x, y, which),
            Type::I64 => {
                self.get(x)?;
                self.get_as(y, Type::I64)?;
                self.op([0x86, 0x88, 0x87][which as usize]);
                Ok(())
            }
            _ => {
                if which == 2 { self.get_s(x)? } else { self.get(x)? }
                self.get_as(y, Type::I32)?;
                if let Some(b) = narrow_bits(t) {
                    self.i32c(b as i32 - 1);
                    self.op(0x71);
                }
                self.op([0x74, 0x76, 0x75][which as usize]);
                self.norm(t);
                Ok(())
            }
        }
    }

    /// i128 shifts (which: 0 shl, 1 ushr, 2 sshr) by n mod 128
    fn shift128(&mut self, x: Value, y: Value, which: u8) -> Result<(), String> {
        let lx = self.loc(x)?;
        let n = self.tmp64(5);
        self.get_as(y, Type::I64)?;
        self.i64c(127);
        self.op(0x83);
        self.lset(n);
        let (lo, hi) = (self.tmp64(6), self.tmp64(7));
        // n == 0: as it is; n < 64: across the halves; else: one half moved by n - 64
        self.lget(n);
        self.op(0x50);
        self.ops(&[0x04, 0x40]);
        self.lget(lx);
        self.lset(lo);
        self.lget(lx + 1);
        self.lset(hi);
        self.op(0x05);
        self.lget(n);
        self.i64c(64);
        self.op(0x54);
        self.ops(&[0x04, 0x40]);
        match which {
            0 => {
                self.lget(lx + 1);
                self.lget(n);
                self.op(0x86);
                self.lget(lx);
                self.i64c(64);
                self.lget(n);
                self.op(0x7D);
                self.op(0x88);
                self.op(0x84);
                self.lset(hi);
                self.lget(lx);
                self.lget(n);
                self.op(0x86);
                self.lset(lo);
            }
            _ => {
                self.lget(lx);
                self.lget(n);
                self.op(0x88);
                self.lget(lx + 1);
                self.i64c(64);
                self.lget(n);
                self.op(0x7D);
                self.op(0x86);
                self.op(0x84);
                self.lset(lo);
                self.lget(lx + 1);
                self.lget(n);
                self.op(if which == 1 { 0x88 } else { 0x87 });
                self.lset(hi);
            }
        }
        self.op(0x05);
        match which {
            0 => {
                self.lget(lx);
                self.lget(n);
                self.i64c(64);
                self.op(0x7D);
                self.op(0x86);
                self.lset(hi);
                self.i64c(0);
                self.lset(lo);
            }
            _ => {
                self.lget(lx + 1);
                self.lget(n);
                self.i64c(64);
                self.op(0x7D);
                self.op(if which == 1 { 0x88 } else { 0x87 });
                self.lset(lo);
                if which == 1 {
                    self.i64c(0);
                } else {
                    self.lget(lx + 1);
                    self.i64c(63);
                    self.op(0x87);
                }
                self.lset(hi);
            }
        }
        self.op(0x0B);
        self.op(0x0B);
        self.lget(lo);
        self.lget(hi);
        Ok(())
    }

    /// i128 add / sub / and / or / xor / mul
    fn op128(&mut self, x: Value, y: Value, which: u8) -> Result<(), String> {
        let (lx, ly) = (self.loc(x)?, self.loc(y)?);
        let lo = self.tmp64(6);
        match which {
            // add: lo = x.lo + y.lo; hi = x.hi + y.hi + (lo <u x.lo)
            0 => {
                self.lget(lx);
                self.lget(ly);
                self.op(0x7C);
                self.ltee(lo);
                self.lget(lx + 1);
                self.lget(ly + 1);
                self.op(0x7C);
                self.lget(lo);
                self.lget(lx);
                self.op(0x54);
                self.op(0xAD);
                self.op(0x7C);
            }
            // sub: lo = x.lo - y.lo; hi = x.hi - y.hi - (x.lo <u y.lo)
            1 => {
                self.lget(lx);
                self.lget(ly);
                self.op(0x7D);
                self.lget(lx + 1);
                self.lget(ly + 1);
                self.op(0x7D);
                self.lget(lx);
                self.lget(ly);
                self.op(0x54);
                self.op(0xAD);
                self.op(0x7D);
            }
            2..=4 => {
                let o = [0x83, 0x84, 0x85][which as usize - 2];
                self.lget(lx);
                self.lget(ly);
                self.op(o);
                self.lget(lx + 1);
                self.lget(ly + 1);
                self.op(o);
            }
            // mul: lo = x.lo * y.lo; hi = umulhi(x.lo, y.lo) + x.lo * y.hi + x.hi * y.lo
            _ => {
                let (a, b) = (self.tmp64(0), self.tmp64(1));
                self.lget(lx);
                self.lset(a);
                self.lget(ly);
                self.lset(b);
                self.lget(lx);
                self.lget(ly);
                self.op(0x7E);
                self.umulhi64();
                self.lget(lx);
                self.lget(ly + 1);
                self.op(0x7E);
                self.op(0x7C);
                self.lget(lx + 1);
                self.lget(ly);
                self.op(0x7E);
                self.op(0x7C);
            }
        }
        Ok(())
    }

    /// (x op y, overflowed) for the *_overflow ops (which: 0 uadd, 1 usub, 2 sadd, 3 ssub, 4 umul, 5 smul)
    fn overflow(&mut self, x: Value, y: Value, which: u8, res: &[u32]) -> Result<(), String> {
        let t = self.ty(x);
        let r = self.tmp64(6);
        match t {
            Type::I64 => {
                let (a, b) = (self.tmp64(0), self.tmp64(1));
                self.get(x)?;
                self.lset(a);
                self.get(y)?;
                self.lset(b);
                self.lget(a);
                self.lget(b);
                self.op([0x7C, 0x7D, 0x7C, 0x7D, 0x7E, 0x7E][which as usize]);
                self.lset(r);
                self.lget(r);
                self.set_id(res[0])?;
                match which {
                    0 => {
                        self.lget(r);
                        self.lget(a);
                        self.op(0x54);
                    }
                    1 => {
                        self.lget(a);
                        self.lget(b);
                        self.op(0x54);
                    }
                    2 | 3 => {
                        // sadd: ((a ^ r) & (b ^ r)) < 0; ssub: ((a ^ b) & (a ^ r)) < 0
                        self.lget(a);
                        if which == 2 {
                            self.lget(r);
                        } else {
                            self.lget(b);
                        }
                        self.op(0x85);
                        if which == 2 {
                            self.lget(b);
                        } else {
                            self.lget(a);
                        }
                        self.lget(r);
                        self.op(0x85);
                        self.op(0x83);
                        self.i64c(0);
                        self.op(0x53);
                    }
                    4 => {
                        self.umulhi64();
                        self.i64c(0);
                        self.op(0x52);
                    }
                    _ => {
                        self.smulhi64();
                        self.lget(r);
                        self.i64c(63);
                        self.op(0x87);
                        self.op(0x52);
                    }
                }
                self.set_id(res[1])?;
            }
            Type::I128 => return Err("wasm32: overflow checks on i128 are not supported yet".into()),
            _ => {
                // computed exactly in i64 (operands extended as their signedness), then compared with the result
                // kept to the type's width
                let bits = narrow_bits(t).unwrap_or(32);
                let signed = matches!(which, 2 | 3 | 5);
                for v in [x, y] {
                    if signed {
                        self.get_s(v)?;
                        self.op(0xAC);
                    } else {
                        self.get(v)?;
                        self.op(0xAD);
                    }
                }
                self.op([0x7C, 0x7D, 0x7C, 0x7D, 0x7E, 0x7E][which as usize]);
                self.lset(r);
                // the result: the low bits
                self.lget(r);
                self.op(0xA7);
                self.norm(t);
                self.set_id(res[0])?;
                // overflowed: the exact value differs from the low bits extended back
                self.lget(r);
                self.lget(r);
                self.i64c(64 - bits as i64);
                self.op(0x86);
                self.i64c(64 - bits as i64);
                self.op(if signed { 0x87 } else { 0x88 });
                self.op(0x52);
                self.set_id(res[1])?;
            }
        }
        Ok(())
    }

    fn fcvt_to_int(&mut self, t: Type, x: Value, signed: bool) -> Result<(), String> {
        let f64 = self.ty(x) == Type::F64;
        self.get(x)?;
        let sub = |w64: bool| -> u8 { (if w64 { 4 } else { 0 }) + if f64 { 2 } else { 0 } + if signed { 0 } else { 1 } };
        match t {
            Type::I64 => self.ops(&[0xFC, sub(true)]),
            Type::I32 => self.ops(&[0xFC, sub(false)]),
            Type::I8 | Type::I16 => {
                // to i32, then clamped to the narrow range
                self.ops(&[0xFC, sub(false)]);
                let b = narrow_bits(t).unwrap();
                let s = self.tmp32(0);
                self.lset(s);
                let (lo, hi) = if signed { (-(1i32 << (b - 1)), (1i32 << (b - 1)) - 1) } else { (0, (1i32 << b) - 1) };
                // max(min(s, hi), lo)
                self.i32c(hi);
                self.lget(s);
                self.lget(s);
                self.i32c(hi);
                self.op(if signed { 0x4A } else { 0x4B });
                self.op(0x1B);
                self.lset(s);
                if signed {
                    self.i32c(lo);
                    self.lget(s);
                    self.lget(s);
                    self.i32c(lo);
                    self.op(0x48);
                    self.op(0x1B);
                    self.lset(s);
                }
                self.lget(s);
                self.norm(t);
            }
            _ => return Err("wasm32: float to i128 is not supported yet".into()),
        }
        Ok(())
    }

    fn bswap(&mut self, x: Value) -> Result<(), String> {
        let t = self.ty(x);
        match t {
            Type::I16 => {
                self.get(x)?;
                self.i32c(8);
                self.op(0x74);
                self.get(x)?;
                self.i32c(8);
                self.op(0x76);
                self.op(0x72);
                self.norm(t);
            }
            Type::I32 | Type::I64 => {
                let n = if t == Type::I32 { 4 } else { 8 };
                let w = t == Type::I64;
                // or of each byte moved to its mirrored place
                for i in 0..n {
                    self.get(x)?;
                    let (sh_r, sh_l) = (8 * i as i64, 8 * (n - 1 - i) as i64);
                    if w {
                        self.i64c(sh_r);
                        self.op(0x88);
                        self.i64c(0xFF);
                        self.op(0x83);
                        self.i64c(sh_l);
                        self.op(0x86);
                    } else {
                        self.i32c(sh_r as i32);
                        self.op(0x76);
                        self.i32c(0xFF);
                        self.op(0x71);
                        self.i32c(sh_l as i32);
                        self.op(0x74);
                    }
                    if i > 0 {
                        self.op(if w { 0x84 } else { 0x72 });
                    }
                }
            }
            Type::I8 => self.get(x)?,
            _ => return Err("wasm32: bswap of this type is not supported".into()),
        }
        Ok(())
    }

    fn rotate(&mut self, x: Value, y: Value, left: bool) -> Result<(), String> {
        let t = self.ty(x);
        match t {
            Type::I32 => {
                self.get(x)?;
                self.get_as(y, Type::I32)?;
                self.op(if left { 0x77 } else { 0x78 });
            }
            Type::I64 => {
                self.get(x)?;
                self.get_as(y, Type::I64)?;
                self.op(if left { 0x89 } else { 0x8A });
            }
            Type::I8 | Type::I16 => {
                let b = narrow_bits(t).unwrap() as i32;
                let n = self.tmp32(1);
                self.get_as(y, Type::I32)?;
                self.i32c(b - 1);
                self.op(0x71);
                self.lset(n);
                // (x << n | x >> (b - n)) for left, mirrored for right; kept to b bits
                self.get(x)?;
                self.lget(n);
                if !left {
                    self.op(0x76);
                } else {
                    self.op(0x74);
                }
                self.get(x)?;
                self.i32c(b);
                self.lget(n);
                self.op(0x6B);
                self.op(if left { 0x76 } else { 0x74 });
                self.op(0x72);
                self.norm(t);
            }
            _ => return Err("wasm32: rotate of i128 is not supported".into()),
        }
        Ok(())
    }

    fn minmax(&mut self, x: Value, y: Value, signed: bool, min: bool) -> Result<(), String> {
        let t = self.ty(x);
        if t == Type::I128 {
            return Err("wasm32: min / max of i128 is not supported".into());
        }
        self.get(x)?;
        self.get(y)?;
        if signed { self.get_s(x)? } else { self.get(x)? }
        if signed { self.get_s(y)? } else { self.get(y)? }
        let cc = match (signed, min) {
            (true, true) => IntCC::SignedLessThan,
            (false, true) => IntCC::UnsignedLessThan,
            (true, false) => IntCC::SignedGreaterThan,
            (false, false) => IntCC::UnsignedGreaterThan,
        };
        self.op(Self::icmp_op(cc, t == Type::I64));
        self.op(0x1B);
        Ok(())
    }

    fn fcmp(&mut self, cc: FloatCC, x: Value, y: Value) -> Result<(), String> {
        let f64 = self.ty(x) == Type::F64;
        if cc == FloatCC::Unordered {
            self.get(x)?;
            self.get(x)?;
            self.op(if f64 { 0x62 } else { 0x5C });
            self.get(y)?;
            self.get(y)?;
            self.op(if f64 { 0x62 } else { 0x5C });
            self.op(0x72);
            return Ok(());
        }
        self.get(x)?;
        self.get(y)?;
        let k = match cc {
            FloatCC::Equal => 0,
            FloatCC::NotEqual => 1,
            FloatCC::LessThan => 2,
            FloatCC::GreaterThan => 3,
            FloatCC::LessThanOrEqual => 4,
            FloatCC::GreaterThanOrEqual => 5,
            FloatCC::Unordered => unreachable!(),
        };
        self.op(if f64 { 0x61 } else { 0x5B } + k);
        Ok(())
    }

    fn funop(&mut self, x: Value, o32: u8, o64: u8) -> Result<(), String> {
        let f64 = self.ty(x) == Type::F64;
        self.get(x)?;
        self.op(if f64 { o64 } else { o32 });
        Ok(())
    }

    fn fbinop(&mut self, x: Value, y: Value, o32: u8, o64: u8) -> Result<(), String> {
        let f64 = self.ty(x) == Type::F64;
        self.get(x)?;
        self.get(y)?;
        self.op(if f64 { o64 } else { o32 });
        Ok(())
    }

    fn extend(&mut self, t: Type, x: Value, signed: bool) -> Result<(), String> {
        let from = self.ty(x);
        if from == t {
            return self.get(x);
        }
        if t == Type::I128 {
            return self.extend128(x, signed);
        }
        if from == Type::I128 {
            // (a reduce through extend: the low half)
            self.get(x)?;
            self.op(0x1A);
            return self.reduce_from(Type::I64, t);
        }
        if from.bits() > t.bits() {
            self.get(x)?;
            return self.reduce_from(from, t);
        }
        if signed { self.get_s(x)? } else { self.get(x)? }
        if t == Type::I64 {
            self.op(if signed { 0xAC } else { 0xAD });
        } else {
            self.norm(t);
        }
        Ok(())
    }

    fn extend128(&mut self, x: Value, signed: bool) -> Result<(), String> {
        let from = self.ty(x);
        let a = self.tmp64(6);
        if signed { self.get_s(x)? } else { self.get(x)? }
        if from != Type::I64 {
            self.op(if signed { 0xAC } else { 0xAD });
        }
        self.ltee(a);
        if signed {
            self.lget(a);
            self.i64c(63);
            self.op(0x87);
        } else {
            self.i64c(0);
        }
        Ok(())
    }

    /// the value of type `from` on the stack, reduced to `t`
    fn reduce_from(&mut self, from: Type, t: Type) -> Result<(), String> {
        if from == Type::I64 && t != Type::I64 {
            self.op(0xA7);
        }
        self.norm(t);
        Ok(())
    }

    fn load(&mut self, t: Type, a: Value, off: i32) -> Result<(), String> {
        self.addr(a, off)?;
        match t {
            Type::I8 => self.op(0x2D),
            Type::I16 => self.op(0x2F),
            Type::I32 => self.op(0x28),
            Type::I64 => self.op(0x29),
            Type::F32 => self.op(0x2A),
            Type::F64 => self.op(0x2B),
            Type::I128 => {
                let p = self.tmp32(0);
                self.ltee(p);
                self.op(0x29);
                self.memarg(0);
                self.lget(p);
                self.op(0x29);
                self.memarg(8);
                return Ok(());
            }
        }
        self.memarg(0);
        Ok(())
    }

    fn store(&mut self, x: Value, a: Value, off: i32) -> Result<(), String> {
        let t = self.ty(x);
        self.addr(a, off)?;
        if t == Type::I128 {
            let p = self.tmp32(0);
            let l = self.loc(x)?;
            self.ltee(p);
            self.lget(l);
            self.op(0x37);
            self.memarg(0);
            self.lget(p);
            self.lget(l + 1);
            self.op(0x37);
            self.memarg(8);
            return Ok(());
        }
        self.get(x)?;
        self.op(match t {
            Type::I8 => 0x3A,
            Type::I16 => 0x3B,
            Type::I32 => 0x36,
            Type::I64 => 0x37,
            Type::F32 => 0x38,
            _ => 0x39,
        });
        self.memarg(0);
        Ok(())
    }
}

/// The ops that do not make values (their results are blocks, variables, slots or function refs)
fn makes_values(op: &Op) -> bool {
    !matches!(op, Op::CreateBlock | Op::DeclareVar(_) | Op::CreateStackSlot(..) | Op::ImportFunction(_))
}

/// One function: the wasm body (after an 8-byte header holding the function's signature, [`sig_enc`]),
/// its relocations (offsets in the returned bytes) and no trap sites (a trap is `unreachable`)
pub fn compile(fb: &FunctionBuilder, zero: &[StackSlot], _opt: bool) -> Result<(Vec<u8>, Vec<Reloc>, Vec<(u32, u8)>), String> {
    let mut pw: Vec<u8> = vec![];
    for &p in &fb.params {
        pw.extend_from_slice(wtys(p));
    }
    let mut rw: Vec<u8> = vec![];
    for &r in &fb.rets {
        rw.extend_from_slice(wtys(r));
    }
    let sig = sig_enc(&pw, &rw)?;
    let mut b = Body {
        fb,
        code: vec![],
        relocs: vec![],
        locals: vec![],
        n_param_locals: pw.len() as u32,
        vloc: vec![],
        var: vec![],
        slot: vec![],
        slot_size: vec![],
        fname: vec![],
        case: vec![],
        n_cases: 0,
        cur_case: 0,
        nest: 0,
        l_label: 0,
        in_loop: false,
        l_fp: 0,
        l_saved_sp: 0,
        frame: 0,
        scratch_i64: vec![],
        scratch_i32: vec![],
        check_addr: std::env::var_os("ZUSTC_WASM_NO_ADDR_CHECK").is_none(),
    };
    // parameters: values 0.. in the first locals
    let mut l = 0u32;
    for (i, &p) in fb.params.iter().enumerate() {
        if b.vloc.len() <= i {
            b.vloc.resize(i + 1, u32::MAX);
        }
        b.vloc[i] = l;
        l += wtys(p).len() as u32;
    }
    // first pass: locals for values and variables, the frame's slots, function names, the order of the blocks
    // (u64: two slots of 2 GiB must not wrap the frame to a small one; a frame past the stack is rejected below)
    let mut frame = 0u64;
    let mut max_align = 16u64;
    let mut first_case: Option<u32> = None;
    let put = |v: &mut Vec<u32>, i: u32, x: u32| {
        let i = i as usize;
        if v.len() <= i {
            v.resize(i + 1, u32::MAX);
        }
        v[i] = x;
    };
    for (i, op) in fb.ops.iter().enumerate() {
        let res = &fb.res[i];
        match op {
            Op::DeclareVar(t) => {
                let ws = wtys(*t);
                let first = b.n_param_locals + b.locals.len() as u32;
                b.locals.extend_from_slice(ws);
                let id = res[0] as usize;
                if b.var.len() <= id {
                    b.var.resize(id + 1, (u32::MAX, Type::I64));
                }
                b.var[id] = (first, *t);
            }
            Op::CreateStackSlot(size, align) => {
                let al = (*align).max(1) as u64;
                max_align = max_align.max(al.next_power_of_two());
                frame = frame.div_ceil(al) * al;
                let id = res[0] as usize;
                if b.slot.len() <= id {
                    b.slot.resize(id + 1, 0);
                    b.slot_size.resize(id + 1, 0);
                }
                b.slot[id] = frame.min(u32::MAX as u64) as u32;
                b.slot_size[id] = *size;
                frame += (*size).max(1) as u64;
            }
            Op::ImportFunction(n) => {
                let id = res[0] as usize;
                if b.fname.len() <= id {
                    b.fname.resize(id + 1, String::new());
                }
                b.fname[id] = n.clone();
            }
            Op::SwitchToBlock(bk) => {
                if b.case.get(bk.0 as usize).is_none_or(|&c| c == u32::MAX) {
                    put(&mut b.case, bk.0, b.n_cases);
                    if first_case.is_none() {
                        first_case = Some(b.n_cases);
                    }
                    b.n_cases += 1;
                }
            }
            _ => {}
        }
        if makes_values(op) && !matches!(op, Op::SwitchToBlock(_)) {
            for &r in res {
                let t = fb.value_type(Value(r));
                let first = b.n_param_locals + b.locals.len() as u32;
                b.locals.extend_from_slice(wtys(t));
                put(&mut b.vloc, r, first);
            }
        }
    }
    let frame = frame.div_ceil(16) * 16;
    if frame > STACK_SIZE {
        return Err(format!("wasm32: a stack frame of {frame} bytes is larger than the shadow stack ({STACK_SIZE} bytes)"));
    }
    b.frame = frame as u32;
    b.l_label = b.new_local(T_I32);
    b.l_fp = b.new_local(T_I64);
    b.l_saved_sp = b.new_local(T_I64);
    // prologue: the frame below the stack pointer (a stack overflow takes it into the null page: a trap)
    if b.frame > 0 {
        b.gget(G_SP);
        b.lset(b.l_saved_sp);
        b.gget(G_SP);
        b.i64c(b.frame as i64);
        b.op(0x7D);
        // (the base aligned down to the strictest slot's alignment: offsets are aligned from it)
        b.i64c(-(max_align as i64));
        b.op(0x83);
        b.ltee(b.l_fp);
        b.gset(G_SP);
        b.lget(b.l_fp);
        b.i64c(NULL_PAGE as i64);
        b.op(0x53);
        b.ops(&[0x04, 0x40, 0x00, 0x0B]);
        // (slots that must start as 0: drop flags)
        for z in zero {
            let (off, size) = (b.slot.get(z.0 as usize).copied().unwrap_or(0), b.slot_size.get(z.0 as usize).copied().unwrap_or(0));
            b.lget(b.l_fp);
            b.i64c(off as i64);
            b.op(0x7C);
            b.op(0xA7);
            b.i32c(0);
            b.i32c(size.max(1) as i32);
            b.ops(&[0xFC, 11, 0]);
        }
    }
    b.i32c(first_case.unwrap_or(0) as i32);
    b.lset(b.l_label);
    // the ops of each block (acg may switch back to a block to go on writing it: a block's ops are those after each
    // switch to it, in order), and the ops before the first block
    let n = b.n_cases;
    let mut per_case: Vec<Vec<usize>> = vec![vec![]; n as usize];
    let mut entry_ops: Vec<usize> = vec![];
    let mut cur: Option<u32> = None;
    for (i, op) in fb.ops.iter().enumerate() {
        if let Op::SwitchToBlock(bk) = op {
            cur = Some(b.case[bk.0 as usize]);
            continue;
        }
        match cur {
            Some(c) => per_case[c as usize].push(i),
            None => entry_ops.push(i),
        }
    }
    for &i in &entry_ops {
        lower_op(&mut b, &fb.ops[i], &fb.res[i])?;
    }
    if n == 0 {
        // (no blocks: straight-line code)
        b.op(0x00);
    } else {
        // the dispatcher: loop { block*n { br_table } case 0 .. case n-1 }
        b.in_loop = true;
        b.ops(&[0x03, 0x40]);
        for _ in 0..n {
            b.ops(&[0x02, 0x40]);
        }
        b.lget(b.l_label);
        b.op(0x0E);
        uleb(&mut b.code, n as u64);
        for k in 0..n {
            uleb(&mut b.code, k as u64);
        }
        uleb(&mut b.code, (n - 1) as u64);
        for c in 0..n {
            b.op(0x0B);
            b.cur_case = c;
            b.nest = 0;
            for &i in &per_case[c as usize] {
                lower_op(&mut b, &fb.ops[i], &fb.res[i])?;
            }
            // (a block that did not end in a jump or return must not run into the next case)
            b.op(0x00);
        }
        b.op(0x0B);
    }
    b.op(0x00);
    b.op(0x0B);
    // header (signature), locals, body
    let mut out = sig.to_le_bytes().to_vec();
    let mut runs: Vec<(u32, u8)> = vec![];
    for &t in &b.locals {
        match runs.last_mut() {
            Some((n, rt)) if *rt == t => *n += 1,
            _ => runs.push((1, t)),
        }
    }
    uleb(&mut out, runs.len() as u64);
    for (n, t) in runs {
        uleb(&mut out, n as u64);
        out.push(t);
    }
    let shift = out.len() as u32;
    if b.n_param_locals + b.locals.len() as u32 > 50000 {
        return Err(format!("wasm32: a function with {} locals (more than engines accept)", b.locals.len()));
    }
    out.extend_from_slice(&b.code);
    let relocs = b.relocs.into_iter().map(|mut r| {
        r.offset += shift;
        r
    }).collect();
    Ok((out, relocs, vec![]))
}

fn lower_op(b: &mut Body, op: &Op, res: &[u32]) -> Result<(), String> {
    match op {
        Op::CreateBlock | Op::AppendBlockParam(..) | Op::DeclareVar(_) | Op::CreateStackSlot(..) | Op::ImportFunction(_) | Op::SwitchToBlock(_) => {}
        Op::DefVar(var, x) => {
            let (l, t) = b.var[var.0 as usize];
            b.get(*x)?;
            if t == Type::I128 {
                b.lset(l + 1);
            }
            b.lset(l);
        }
        Op::UseVar(var) => {
            let (l, t) = b.var[var.0 as usize];
            b.lget(l);
            if t == Type::I128 {
                b.lget(l + 1);
            }
            b.set_id(res[0])?;
        }
        Op::Iconst(t, x) => {
            match t {
                Type::I128 => {
                    b.i64c(*x);
                    b.i64c(if *x < 0 { -1 } else { 0 });
                }
                Type::I64 => b.i64c(*x),
                Type::I32 => b.i32c(*x as i32),
                Type::I8 | Type::I16 => b.i32c((*x & ((1i64 << narrow_bits(*t).unwrap()) - 1)) as i32),
                _ => return Err("wasm32: iconst of a float type".into()),
            }
            b.set_id(res[0])?;
        }
        Op::IaddImm(x, k) => {
            let t = b.ty(*x);
            b.get(*x)?;
            if t == Type::I64 {
                b.i64c(*k);
                b.op(0x7C);
            } else if t == Type::I128 {
                return Err("wasm32: iadd_imm on i128".into());
            } else {
                b.i32c(*k as i32);
                b.op(0x6A);
                b.norm(t);
            }
            b.set_id(res[0])?;
        }
        Op::Iadd(x, y) | Op::Iadd128(x, y) if b.ty(*x) == Type::I128 => {
            b.op128(*x, *y, 0)?;
            b.set_id(res[0])?;
        }
        Op::Isub(x, y) | Op::Isub128(x, y) if b.ty(*x) == Type::I128 => {
            b.op128(*x, *y, 1)?;
            b.set_id(res[0])?;
        }
        Op::Band(x, y) | Op::Band128(x, y) if b.ty(*x) == Type::I128 => {
            b.op128(*x, *y, 2)?;
            b.set_id(res[0])?;
        }
        Op::Bor(x, y) | Op::Bor128(x, y) if b.ty(*x) == Type::I128 => {
            b.op128(*x, *y, 3)?;
            b.set_id(res[0])?;
        }
        Op::Bxor(x, y) | Op::Bxor128(x, y) if b.ty(*x) == Type::I128 => {
            b.op128(*x, *y, 4)?;
            b.set_id(res[0])?;
        }
        Op::Imul(x, y) | Op::Imul128(x, y) if b.ty(*x) == Type::I128 => {
            b.op128(*x, *y, 5)?;
            b.set_id(res[0])?;
        }
        Op::Iadd(x, y) | Op::Iadd128(x, y) => {
            b.binop(*x, *y, 0x6A, 0x7C, false)?;
            b.set_id(res[0])?;
        }
        Op::Isub(x, y) | Op::Isub128(x, y) => {
            b.binop(*x, *y, 0x6B, 0x7D, false)?;
            b.set_id(res[0])?;
        }
        Op::Imul(x, y) | Op::Imul128(x, y) => {
            b.binop(*x, *y, 0x6C, 0x7E, false)?;
            b.set_id(res[0])?;
        }
        Op::Band(x, y) | Op::Band128(x, y) => {
            b.binop(*x, *y, 0x71, 0x83, false)?;
            b.set_id(res[0])?;
        }
        Op::Bor(x, y) | Op::Bor128(x, y) => {
            b.binop(*x, *y, 0x72, 0x84, false)?;
            b.set_id(res[0])?;
        }
        Op::Bxor(x, y) | Op::Bxor128(x, y) => {
            b.binop(*x, *y, 0x73, 0x85, false)?;
            b.set_id(res[0])?;
        }
        Op::Udiv(x, y) => {
            b.binop(*x, *y, 0x6E, 0x80, false)?;
            b.set_id(res[0])?;
        }
        Op::Sdiv(x, y) => {
            b.binop(*x, *y, 0x6D, 0x7F, true)?;
            b.set_id(res[0])?;
        }
        Op::Urem(x, y) => {
            b.binop(*x, *y, 0x70, 0x82, false)?;
            b.set_id(res[0])?;
        }
        Op::Srem(x, y) => {
            b.binop(*x, *y, 0x6F, 0x81, true)?;
            b.set_id(res[0])?;
        }
        Op::Ishl(x, y) | Op::Ishl128(x, y) => {
            b.shift(*x, *y, 0)?;
            b.set_id(res[0])?;
        }
        Op::Ushr(x, y) | Op::Ushr128(x, y) => {
            b.shift(*x, *y, 1)?;
            b.set_id(res[0])?;
        }
        Op::Sshr(x, y) | Op::Sshr128(x, y) => {
            b.shift(*x, *y, 2)?;
            b.set_id(res[0])?;
        }
        Op::Ineg(x) => {
            let t = b.ty(*x);
            match t {
                Type::I64 => {
                    b.i64c(0);
                    b.get(*x)?;
                    b.op(0x7D);
                }
                Type::I128 => {
                    let l = b.loc(*x)?;
                    // 0 - x
                    b.i64c(0);
                    b.lget(l);
                    b.op(0x7D);
                    b.i64c(0);
                    b.lget(l + 1);
                    b.op(0x7D);
                    b.lget(l);
                    b.i64c(0);
                    b.op(0x52);
                    b.op(0xAD);
                    b.op(0x7D);
                }
                _ => {
                    b.i32c(0);
                    b.get(*x)?;
                    b.op(0x6B);
                    b.norm(t);
                }
            }
            b.set_id(res[0])?;
        }
        Op::Bnot(x) => {
            let t = b.ty(*x);
            match t {
                Type::I64 => {
                    b.get(*x)?;
                    b.i64c(-1);
                    b.op(0x85);
                }
                Type::I128 => {
                    let l = b.loc(*x)?;
                    b.lget(l);
                    b.i64c(-1);
                    b.op(0x85);
                    b.lget(l + 1);
                    b.i64c(-1);
                    b.op(0x85);
                }
                _ => {
                    b.get(*x)?;
                    b.i32c(-1);
                    b.op(0x73);
                    b.norm(t);
                }
            }
            b.set_id(res[0])?;
        }
        Op::Icmp(cc, x, y) | Op::Icmp128(cc, x, y) => {
            b.icmp(*cc, *x, Some((*y, 0)))?;
            b.set_id(res[0])?;
        }
        Op::IcmpImm(cc, x, k) => {
            b.icmp_imm(*cc, *x, *k)?;
            b.set_id(res[0])?;
        }
        Op::Select(c, x, y) => {
            if b.ty(*x) == Type::I128 {
                let (lx, ly) = (b.loc(*x)?, b.loc(*y)?);
                for h in 0..2 {
                    b.lget(lx + h);
                    b.lget(ly + h);
                    b.cond(*c)?;
                    b.op(0x1B);
                }
            } else {
                b.get(*x)?;
                b.get(*y)?;
                b.cond(*c)?;
                b.op(0x1B);
            }
            b.set_id(res[0])?;
        }
        Op::Uextend(t, x) => {
            b.extend(*t, *x, false)?;
            b.set_id(res[0])?;
        }
        Op::Sextend(t, x) => {
            b.extend(*t, *x, true)?;
            b.set_id(res[0])?;
        }
        Op::Ireduce(t, x) | Op::Reduce128(t, x) => {
            let from = b.ty(*x);
            b.get(*x)?;
            if from == Type::I128 {
                b.op(0x1A);
                b.reduce_from(Type::I64, *t)?;
            } else {
                b.reduce_from(from, *t)?;
            }
            b.set_id(res[0])?;
        }
        Op::Extend128(x, signed) => {
            b.extend128(*x, *signed)?;
            b.set_id(res[0])?;
        }
        Op::Load(t, a, off) => {
            b.load(*t, *a, *off)?;
            b.set_id(res[0])?;
        }
        Op::Store(x, a, off) => b.store(*x, *a, *off)?,
        Op::StackAddr(s, off) => {
            let so = b.slot.get(s.0 as usize).copied().unwrap_or(0) as i64;
            b.lget(b.l_fp);
            b.i64c(so + *off as i64);
            b.op(0x7C);
            b.set_id(res[0])?;
        }
        Op::Jump(bk, args) => b.jump(bk.0, args)?,
        Op::Brif(c, tb, ta, eb, ea) => {
            b.cond(*c)?;
            b.ops(&[0x04, 0x40]);
            b.nest += 1;
            b.jump(tb.0, ta)?;
            b.op(0x05);
            b.jump(eb.0, ea)?;
            b.op(0x0B);
            b.nest -= 1;
            b.op(0x00);
        }
        Op::Return_(rs) => b.ret(rs)?,
        Op::Trap | Op::TrapCode(_) => b.op(0x00),
        Op::Trapz(c) | Op::TrapzCode(c, _) => b.trap_if(*c, true)?,
        Op::Trapnz(c) | Op::TrapnzCode(c, _) => b.trap_if(*c, false)?,
        Op::Call(f, args, rets) => {
            let name = b.fname.get(f.0 as usize).cloned().unwrap_or_default();
            match name.as_str() {
                // (bulk memory, for acg's own calls by name (struct copies): in place. Calls through the prelude's
                // extern declarations are named only when the module is written, and go to the built-in functions)
                "memcpy" | "memmove" | "memset" if args.len() == 3 => {
                    // (the operands checked as a load's address is: a bulk operation must not bypass the checks)
                    if b.check_addr {
                        let ptrs: &[Value] = if name == "memset" { &args[..1] } else { &args[..2] };
                        b.check_range(ptrs, args[2])?;
                    }
                    let d = b.tmp32(2);
                    b.get(args[0])?;
                    b.op(0xA7);
                    b.ltee(d);
                    if name == "memset" {
                        b.get(args[1])?;
                        b.get_as(args[2], Type::I32)?;
                        b.ops(&[0xFC, 11, 0]);
                    } else {
                        b.get(args[1])?;
                        b.op(0xA7);
                        b.get_as(args[2], Type::I32)?;
                        b.ops(&[0xFC, 10, 0, 0]);
                    }
                    if let Some(&r) = res.first() {
                        b.get(args[0])?;
                        b.set_id(r)?;
                    }
                }
                _ => {
                    let mut pw = vec![];
                    for &a in args {
                        b.get(a)?;
                        pw.extend_from_slice(wtys(b.ty(a)));
                    }
                    let mut rw = vec![];
                    for &t in rets {
                        rw.extend_from_slice(wtys(t));
                    }
                    let sig = sig_enc(&pw, &rw)?;
                    b.call_reloc(&name, sig);
                    for &r in res.iter().rev() {
                        b.set_id(r)?;
                    }
                }
            }
        }
        Op::CallIndirect(p, args, rets) => {
            let mut pw = vec![];
            for &a in args {
                b.get(a)?;
                pw.extend_from_slice(wtys(b.ty(a)));
            }
            let mut rw = vec![];
            for &t in rets {
                rw.extend_from_slice(wtys(t));
            }
            let sig = sig_enc(&pw, &rw)?;
            // (a function pointer is a table index: one past 32 bits would wrap onto another entry. A trap)
            let t = b.tmp64(9);
            b.get(*p)?;
            b.ltee(t);
            b.i64c(0xFFFF_FFFF);
            b.op(0x56);
            b.ops(&[0x04, 0x40, 0x00, 0x0B]);
            b.lget(t);
            b.op(0xA7);
            b.op(0x11);
            let at = b.code.len();
            b.code.extend_from_slice(&[0; 5]);
            uleb5_at(&mut b.code[at..at + 5], 0);
            b.relocs.push(Reloc { offset: at as u32, name: String::new(), kind: R_TYPE, addend: sig });
            b.op(0x00);
            for &r in res.iter().rev() {
                b.set_id(r)?;
            }
        }
        Op::SymbolAddr(n) => {
            b.op(0x42);
            let at = b.code.len();
            b.code.extend_from_slice(&[0; 10]);
            sleb10_at(&mut b.code[at..at + 10], 0);
            b.relocs.push(Reloc { offset: at as u32, name: n.clone(), kind: R_ADDR, addend: 0 });
            b.set_id(res[0])?;
        }
        Op::F64const(x) => {
            b.op(0x44);
            b.code.extend_from_slice(&x.to_le_bytes());
            b.set_id(res[0])?;
        }
        Op::F32const(x) => {
            b.op(0x43);
            b.code.extend_from_slice(&x.to_le_bytes());
            b.set_id(res[0])?;
        }
        Op::Fadd(x, y) => {
            b.fbinop(*x, *y, 0x92, 0xA0)?;
            b.set_id(res[0])?;
        }
        Op::Fsub(x, y) => {
            b.fbinop(*x, *y, 0x93, 0xA1)?;
            b.set_id(res[0])?;
        }
        Op::Fmul(x, y) => {
            b.fbinop(*x, *y, 0x94, 0xA2)?;
            b.set_id(res[0])?;
        }
        Op::Fdiv(x, y) => {
            b.fbinop(*x, *y, 0x95, 0xA3)?;
            b.set_id(res[0])?;
        }
        Op::Fcopysign(x, y) => {
            b.fbinop(*x, *y, 0x98, 0xA6)?;
            b.set_id(res[0])?;
        }
        Op::Sqrt(x) => {
            b.funop(*x, 0x91, 0x9F)?;
            b.set_id(res[0])?;
        }
        Op::Floor(x) => {
            b.funop(*x, 0x8E, 0x9C)?;
            b.set_id(res[0])?;
        }
        Op::Ceil(x) => {
            b.funop(*x, 0x8D, 0x9B)?;
            b.set_id(res[0])?;
        }
        Op::Trunc(x) => {
            b.funop(*x, 0x8F, 0x9D)?;
            b.set_id(res[0])?;
        }
        Op::Fneg(x) => {
            b.funop(*x, 0x8C, 0x9A)?;
            b.set_id(res[0])?;
        }
        Op::Fabs(x) => {
            b.funop(*x, 0x8B, 0x99)?;
            b.set_id(res[0])?;
        }
        Op::Fma(x, y, z) => {
            // (not fused: wasm has no fma. For f32 the product is exact in f64)
            if b.ty(*x) == Type::F32 {
                b.get(*x)?;
                b.op(0xBB);
                b.get(*y)?;
                b.op(0xBB);
                b.op(0xA2);
                b.get(*z)?;
                b.op(0xBB);
                b.op(0xA0);
                b.op(0xB6);
            } else {
                b.get(*x)?;
                b.get(*y)?;
                b.op(0xA2);
                b.get(*z)?;
                b.op(0xA0);
            }
            b.set_id(res[0])?;
        }
        Op::Bitcast(t, x) => {
            let from = b.ty(*x);
            b.get(*x)?;
            match (from, *t) {
                (a, c) if a == c => {}
                (Type::F32, Type::I32) => b.op(0xBC),
                (Type::F64, Type::I64) => b.op(0xBD),
                (Type::I32, Type::F32) => b.op(0xBE),
                (Type::I64, Type::F64) => b.op(0xBF),
                (a, c) => return Err(format!("wasm32: bitcast {a:?} to {c:?}")),
            }
            b.set_id(res[0])?;
        }
        Op::Fcmp(cc, x, y) => {
            b.fcmp(*cc, *x, *y)?;
            b.set_id(res[0])?;
        }
        Op::Fpromote(x) => {
            b.get(*x)?;
            b.op(0xBB);
            b.set_id(res[0])?;
        }
        Op::Fdemote(x) => {
            b.get(*x)?;
            b.op(0xB6);
            b.set_id(res[0])?;
        }
        Op::FcvtFromSint(t, x) | Op::FcvtFromUint(t, x) => {
            let signed = matches!(op, Op::FcvtFromSint(..));
            let from = b.ty(*x);
            if from == Type::I128 {
                return Err("wasm32: i128 to float is not supported yet".into());
            }
            if signed { b.get_s(*x)? } else { b.get(*x)? }
            let w64 = from == Type::I64;
            let f64 = *t == Type::F64;
            b.op(match (f64, w64, signed) {
                (false, false, true) => 0xB2,
                (false, false, false) => 0xB3,
                (false, true, true) => 0xB4,
                (false, true, false) => 0xB5,
                (true, false, true) => 0xB7,
                (true, false, false) => 0xB8,
                (true, true, true) => 0xB9,
                (true, true, false) => 0xBA,
            });
            b.set_id(res[0])?;
        }
        Op::FcvtToSintSat(t, x) => {
            b.fcvt_to_int(*t, *x, true)?;
            b.set_id(res[0])?;
        }
        Op::FcvtToUintSat(t, x) => {
            b.fcvt_to_int(*t, *x, false)?;
            b.set_id(res[0])?;
        }
        Op::Iconcat(lo, hi) => {
            b.get_as(*lo, Type::I64)?;
            b.get_as(*hi, Type::I64)?;
            b.set_id(res[0])?;
        }
        Op::Isplit(x) => {
            let l = b.loc(*x)?;
            b.lget(l);
            b.set_id(res[0])?;
            b.lget(l + 1);
            b.set_id(res[1])?;
        }
        Op::Udiv128(..) | Op::Sdiv128(..) | Op::Urem128(..) | Op::Srem128(..) => {
            return Err("wasm32: i128 division is not supported yet".into());
        }
        Op::UaddOverflow(x, y) => b.overflow(*x, *y, 0, res)?,
        Op::UsubOverflow(x, y) => b.overflow(*x, *y, 1, res)?,
        Op::SaddOverflow(x, y) => b.overflow(*x, *y, 2, res)?,
        Op::SsubOverflow(x, y) => b.overflow(*x, *y, 3, res)?,
        Op::UmulOverflow(x, y) => b.overflow(*x, *y, 4, res)?,
        Op::SmulOverflow(x, y) => b.overflow(*x, *y, 5, res)?,
        Op::Umulhi(x, y) => {
            let t = b.ty(*x);
            match t {
                Type::I64 => {
                    let (a, c) = (b.tmp64(0), b.tmp64(1));
                    b.get(*x)?;
                    b.lset(a);
                    b.get(*y)?;
                    b.lset(c);
                    b.umulhi64();
                }
                Type::I128 => return Err("wasm32: umulhi of i128".into()),
                _ => {
                    let bits = narrow_bits(t).unwrap_or(32) as i64;
                    b.get(*x)?;
                    b.op(0xAD);
                    b.get(*y)?;
                    b.op(0xAD);
                    b.op(0x7E);
                    b.i64c(bits);
                    b.op(0x88);
                    b.op(0xA7);
                    b.norm(t);
                }
            }
            b.set_id(res[0])?;
        }
        Op::Clz(x) | Op::Ctz(x) | Op::Popcnt(x) => {
            let t = b.ty(*x);
            let which = match op {
                Op::Clz(_) => 0,
                Op::Ctz(_) => 1,
                _ => 2,
            };
            match t {
                Type::I64 => {
                    b.get(*x)?;
                    b.op(0x79 + which);
                }
                Type::I128 => return Err("wasm32: bit counts of i128 are not supported yet".into()),
                _ => {
                    let bits = narrow_bits(t).unwrap_or(32) as i32;
                    b.get(*x)?;
                    if which == 1 && bits < 32 {
                        b.i32c(1 << bits);
                        b.op(0x72);
                    }
                    b.op(0x67 + which);
                    if which == 0 && bits < 32 {
                        b.i32c(32 - bits);
                        b.op(0x6B);
                    }
                }
            }
            b.set_id(res[0])?;
        }
        Op::Bswap(x) => {
            b.bswap(*x)?;
            b.set_id(res[0])?;
        }
        Op::Rotl(x, y) => {
            b.rotate(*x, *y, true)?;
            b.set_id(res[0])?;
        }
        Op::Rotr(x, y) => {
            b.rotate(*x, *y, false)?;
            b.set_id(res[0])?;
        }
        Op::Smin(x, y) => {
            b.minmax(*x, *y, true, true)?;
            b.set_id(res[0])?;
        }
        Op::Umin(x, y) => {
            b.minmax(*x, *y, false, true)?;
            b.set_id(res[0])?;
        }
        Op::Smax(x, y) => {
            b.minmax(*x, *y, true, false)?;
            b.set_id(res[0])?;
        }
        Op::Umax(x, y) => {
            b.minmax(*x, *y, false, false)?;
            b.set_id(res[0])?;
        }
        Op::Iabs(x) => {
            let t = b.ty(*x);
            match t {
                Type::I64 => {
                    b.i64c(0);
                    b.get(*x)?;
                    b.op(0x7D);
                    b.get(*x)?;
                    b.get(*x)?;
                    b.i64c(0);
                    b.op(0x53);
                    b.op(0x1B);
                }
                Type::I128 => return Err("wasm32: abs of i128".into()),
                _ => {
                    b.i32c(0);
                    b.get_s(*x)?;
                    b.op(0x6B);
                    b.get_s(*x)?;
                    b.get_s(*x)?;
                    b.i32c(0);
                    b.op(0x48);
                    b.op(0x1B);
                    b.norm(t);
                }
            }
            b.set_id(res[0])?;
        }
    }
    Ok(())
}

// --- the module -------------------------------------------------------------------------------------------

/// A function as build.rs lays them out: (symbol, code from [`compile`], relocations, exported)
pub type FuncDef = (String, Vec<u8>, Vec<Reloc>, bool);
/// A data object: (symbol, bytes, relocations (offset, symbol, addend), exported)
pub type DataDef = (String, Vec<u8>, Vec<(u32, String, i64)>, bool);

/// The functions the module provides itself when the program calls them (instead of importing them)
const BUILTINS: &[&str] = &["malloc", "free", "realloc", "calloc", "__errno_location", "strerror", "strlen", "memcpy", "memmove", "memset"];

/// The wasm target a module is written for
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WasmTarget {
    /// wasm32-unknown-unknown: library-like, `env` imports, exports `memory` and the flagged functions
    Unknown,
    /// wasm32-wasip1: WASI preview1 command module (`wasi_snapshot_preview1` imports, a synthesized `_start`)
    WasiP1,
    /// wasm32-wasip2: currently the same preview1-compatible core module as wasip1
    WasiP2,
}

/// Extra functions a WASI module provides itself: stdio / process over `fd_write` / `proc_exit`, and no-op or
/// pure shims for what the std runtime calls but a sandbox has no host for (signals, stdin, `round`, `memmem`)
const WASI_SHIMS: &[&str] = &["putchar", "write", "exit", "abort", "fflush", "getchar", "read", "close", "sigaction", "__exr_trap_at", "memmem", "round", "roundf"];

/// Where the built-ins keep their state: the free-list heads, errno, strerror's text, and (WASI) the iovec scratch
struct Layout {
    free_base: i64,
    errno: i64,
    msg: i64,
    /// WASI only: a 32-byte scratch for `fd_write`'s iovec (buf, len) and the nwritten out-parameter
    scratch: i64,
}

/// strerror's one text (the host gives no error numbers)
const ERR_TEXT: &[u8] = b"operation not permitted in the wasm sandbox\0";

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn name_bytes(out: &mut Vec<u8>, s: &str) {
    uleb(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// A small assembler for the built-in functions (i64 locals and parameters)
struct Asm {
    c: Vec<u8>,
}

impl Asm {
    fn o(&mut self, bs: &[u8]) -> &mut Self {
        self.c.extend_from_slice(bs);
        self
    }
    fn get(&mut self, l: u32) -> &mut Self {
        self.c.push(0x20);
        uleb(&mut self.c, l as u64);
        self
    }
    fn set(&mut self, l: u32) -> &mut Self {
        self.c.push(0x21);
        uleb(&mut self.c, l as u64);
        self
    }
    fn k(&mut self, x: i64) -> &mut Self {
        self.c.push(0x42);
        sleb(&mut self.c, x);
        self
    }
    fn call(&mut self, f: u32) -> &mut Self {
        self.c.push(0x10);
        uleb(&mut self.c, f as u64);
        self
    }
    fn g(&mut self, i: u32) -> &mut Self {
        self.c.push(0x23);
        uleb(&mut self.c, i as u64);
        self
    }
    fn gs(&mut self, i: u32) -> &mut Self {
        self.c.push(0x24);
        uleb(&mut self.c, i as u64);
        self
    }
    /// i64.load / i64.store at the i64 address on the stack (wrapped)
    fn load(&mut self) -> &mut Self {
        self.o(&[0x29, 0, 0])
    }
    fn wrap(&mut self) -> &mut Self {
        self.o(&[0xA7])
    }
}

/// The address checks of Body::addr for a built-in's pointer parameters: each must be past the null page and fit in
/// 32 bits, unless the count parameter is 0; the count must fit in 32 bits
fn check_bulk(a: &mut Asm, ptrs: &[u32], count: Option<u32>) {
    if let Some(n) = count {
        a.get(n).k(0xFFFF_FFFF).o(&[0x56, 0x04, 0x40, 0x00, 0x0B]);
        a.get(n).o(&[0x50, 0x45, 0x04, 0x40]);
    }
    for &p in ptrs {
        a.get(p).k(NULL_PAGE as i64).o(&[0x7D]).k(0xFFFF_FFFF - NULL_PAGE as i64).o(&[0x56, 0x04, 0x40, 0x00, 0x0B]);
    }
    if count.is_some() {
        a.o(&[0x0B]);
    }
}

/// The bodies of the built-in allocator (a power-of-two size-class free list over a bump heap).
/// Blocks: 16 bytes of header ([0] the next free block while free, [8] the class), then the user's bytes.
/// `free_base` is the address of the free-list heads (64 classes of 8 bytes). idx: the functions' indices
fn builtin_body(name: &str, lay: &Layout, idx: &dyn Fn(&str) -> u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let free_base = lay.free_base;
    let mut a = Asm { c: vec![] };
    // (params, results, locals)
    let (params, rets, locals) = match name {
        // malloc(n) -> p. locals: 0 n, 1 k, 2 p, 3 head, 4 top
        "malloc" => {
            a.get(0).o(&[0x50, 0x04, 0x40]).k(1).set(0).o(&[0x0B]);
            // n > MAX_ALLOC (unsigned): null
            a.get(0).k(MAX_ALLOC).o(&[0x56, 0x04, 0x40]).k(0).o(&[0x0F, 0x0B]);
            // k = max(5, 64 - clz(n + 16 - 1))
            a.k(64).get(0).k(15).o(&[0x7C, 0x79, 0x7D]).set(1);
            a.get(1).k(5).o(&[0x54, 0x04, 0x40]).k(5).set(1).o(&[0x0B]);
            // head = free_base + k * 8; p = *head
            a.k(free_base).get(1).k(3).o(&[0x86, 0x7C]).set(3);
            a.get(3).wrap().load().set(2);
            a.get(2).o(&[0x50, 0x45, 0x04, 0x40]);
            // a free block: *head = p.next
            a.get(3).wrap().get(2).wrap().load().o(&[0x37, 0, 0]);
            a.o(&[0x05]);
            // a new block from the top of the heap, growing memory when needed
            a.g(G_HEAP).set(2);
            a.get(2).k(1).get(1).o(&[0x86, 0x7C]).set(4);
            a.get(4).o(&[0x3F, 0x00, 0xAD]).k(16).o(&[0x86, 0x55, 0x04, 0x40]);
            // pages = (top - size_bytes + 65535) >> 16
            a.get(4).o(&[0x3F, 0x00, 0xAD]).k(16).o(&[0x86, 0x7D]).k(65535).o(&[0x7C]).k(16).o(&[0x88]).wrap();
            a.o(&[0x40, 0x00]).o(&[0x41]);
            sleb(&mut a.c, -1);
            a.o(&[0x46, 0x04, 0x40]).k(0).o(&[0x0F, 0x0B]);
            a.o(&[0x0B]);
            a.get(4).gs(G_HEAP);
            a.o(&[0x0B]);
            // header: the class; the user's bytes after it
            a.get(2).k(8).o(&[0x7C]).wrap().get(1).o(&[0x37, 0, 0]);
            a.get(2).k(16).o(&[0x7C]);
            (vec![T_I64], vec![T_I64], vec![T_I64; 4])
        }
        // free(p). locals: 0 p, 1 b, 2 head
        "free" => {
            a.get(0).o(&[0x50, 0x04, 0x40, 0x0F, 0x0B]);
            a.get(0).k(16).o(&[0x7D]).set(1);
            a.k(free_base).get(1).k(8).o(&[0x7C]).wrap().load().k(3).o(&[0x86, 0x7C]).set(2);
            a.get(1).wrap().get(2).wrap().load().o(&[0x37, 0, 0]);
            a.get(2).wrap().get(1).o(&[0x37, 0, 0]);
            (vec![T_I64], vec![], vec![T_I64; 2])
        }
        // realloc(p, n) -> q. locals: 0 p, 1 n, 2 cap, 3 q
        "realloc" => {
            a.get(0).o(&[0x50, 0x04, 0x40]).get(1).call(idx("malloc")).o(&[0x0F, 0x0B]);
            a.k(1).get(0).k(8).o(&[0x7D]).wrap().load().o(&[0x86]).k(16).o(&[0x7D]).set(2);
            a.get(1).get(2).o(&[0x58, 0x04, 0x40]).get(0).o(&[0x0F, 0x0B]);
            a.get(1).call(idx("malloc")).set(3);
            a.get(3).o(&[0x50, 0x04, 0x40]).k(0).o(&[0x0F, 0x0B]);
            a.get(3).wrap().get(0).wrap().get(2).wrap().o(&[0xFC, 10, 0, 0]);
            a.get(0).call(idx("free"));
            a.get(3);
            (vec![T_I64, T_I64], vec![T_I64], vec![T_I64; 2])
        }
        // memcpy / memmove (d, s, n) -> d: memory.copy handles overlap
        "memcpy" | "memmove" => {
            check_bulk(&mut a, &[0, 1], Some(2));
            a.get(0).wrap().get(1).wrap().get(2).wrap().o(&[0xFC, 10, 0, 0]).get(0);
            (vec![T_I64, T_I64, T_I64], vec![T_I64], vec![])
        }
        "memset" => {
            check_bulk(&mut a, &[0], Some(2));
            a.get(0).wrap().get(1).get(2).wrap().o(&[0xFC, 11, 0]).get(0);
            (vec![T_I64, T_I32, T_I64], vec![T_I64], vec![])
        }
        "__errno_location" => {
            a.k(lay.errno);
            (vec![], vec![T_I64], vec![])
        }
        "strerror" => {
            a.k(lay.msg);
            (vec![T_I32], vec![T_I64], vec![])
        }
        // strlen(p) -> n. locals: 0 p, 1 n
        "strlen" => {
            check_bulk(&mut a, &[0], None);
            a.o(&[0x02, 0x40, 0x03, 0x40]).get(0).get(1).o(&[0x7C]).wrap().o(&[0x2D, 0, 0, 0x45, 0x0D, 1]);
            a.get(1).k(1).o(&[0x7C]).set(1).o(&[0x0C, 0, 0x0B, 0x0B]).get(1);
            (vec![T_I64], vec![T_I64], vec![T_I64])
        }
        // calloc(m, n) -> p (null when m * n overflows). locals: 0 m, 1 n, 2 size, 3 p
        _ => {
            a.get(0).get(1).o(&[0x7E]).set(2);
            a.get(1).o(&[0x50, 0x45, 0x04, 0x40]).get(2).get(1).o(&[0x80]).get(0).o(&[0x52, 0x04, 0x40]).k(0).o(&[0x0F, 0x0B, 0x0B]);
            a.get(2).call(idx("malloc")).set(3);
            a.get(3).o(&[0x50, 0x45, 0x04, 0x40]).get(3).wrap().o(&[0x41, 0]).get(2).wrap().o(&[0xFC, 11, 0, 0x0B]);
            a.get(3);
            (vec![T_I64, T_I64], vec![T_I64], vec![T_I64; 2])
        }
    };
    // (the body: locals, code, end)
    let mut body = vec![];
    if locals.is_empty() {
        uleb(&mut body, 0);
    } else {
        uleb(&mut body, 1);
        uleb(&mut body, locals.len() as u64);
        body.push(T_I64);
    }
    body.extend_from_slice(&a.c);
    body.push(0x0B);
    (params, rets, body)
}

/// The bodies of the WASI shims: stdio / process over the `fd_write` / `proc_exit` imports, no-ops for what a
/// sandbox has no host for, and pure `memmem` / `round` / `roundf`. `idx` resolves the WASI imports' function indices
fn wasi_shim_body(name: &str, lay: &Layout, idx: &dyn Fn(&str) -> u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let s = lay.scratch;
    let mut a = Asm { c: vec![] };
    // (params, results, local groups, code)
    let (params, rets, locals, code): (Vec<u8>, Vec<u8>, Vec<(u32, u8)>, Vec<u8>) = match name {
        // putchar(c) -> c: store the byte, build a one-byte iovec, fd_write(1, iov, 1, &nwritten)
        "putchar" => {
            a.o(&[0x41]); sleb(&mut a.c, s); a.o(&[0x20, 0x00, 0x3A, 0x00, 0x00]); // [s] = c
            a.o(&[0x41]); sleb(&mut a.c, s + 8); a.o(&[0x41]); sleb(&mut a.c, s); a.o(&[0x36, 0x02, 0x00]); // iov.buf = s
            a.o(&[0x41]); sleb(&mut a.c, s + 12); a.o(&[0x41, 0x01, 0x36, 0x02, 0x00]); // iov.len = 1
            a.o(&[0x41, 0x01, 0x41]); sleb(&mut a.c, s + 8); a.o(&[0x41, 0x01, 0x41]); sleb(&mut a.c, s + 16);
            a.call(idx("fd_write"));
            a.o(&[0x1A, 0x20, 0x00]); // drop nwritten; return c
            (vec![T_I32], vec![T_I32], vec![], std::mem::take(&mut a.c))
        }
        // write(fd, p, n) -> n: a one-iovec fd_write of the caller's buffer
        "write" => {
            a.o(&[0x41]); sleb(&mut a.c, s + 8); a.o(&[0x20, 0x01, 0xA7, 0x36, 0x02, 0x00]); // iov.buf = p
            a.o(&[0x41]); sleb(&mut a.c, s + 12); a.o(&[0x20, 0x02, 0xA7, 0x36, 0x02, 0x00]); // iov.len = n
            a.o(&[0x20, 0x00, 0x41]); sleb(&mut a.c, s + 8); a.o(&[0x41, 0x01, 0x41]); sleb(&mut a.c, s + 16);
            a.call(idx("fd_write"));
            a.o(&[0x1A, 0x20, 0x02]); // drop nwritten; return n
            (vec![T_I32, T_I64, T_I64], vec![T_I64], vec![], std::mem::take(&mut a.c))
        }
        "exit" => { a.get(0); a.call(idx("proc_exit")); (vec![T_I32], vec![], vec![], std::mem::take(&mut a.c)) }
        "abort" => { a.o(&[0x41, 0x86, 0x01]); a.call(idx("proc_exit")); (vec![], vec![], vec![], std::mem::take(&mut a.c)) } // 134
        // no host for signals or stdin: fflush / close / sigaction succeed, getchar / read are EOF
        "fflush" => { a.o(&[0x41, 0x00]); (vec![T_I64], vec![T_I32], vec![], std::mem::take(&mut a.c)) }
        "getchar" => { a.o(&[0x41, 0x7F]); (vec![], vec![T_I32], vec![], std::mem::take(&mut a.c)) } // -1
        "read" => { a.k(0); (vec![T_I32, T_I64, T_I64], vec![T_I64], vec![], std::mem::take(&mut a.c)) }
        "close" => { a.o(&[0x41, 0x00]); (vec![T_I32], vec![T_I32], vec![], std::mem::take(&mut a.c)) }
        "sigaction" => { a.o(&[0x41, 0x00]); (vec![T_I32, T_I64, T_I64], vec![T_I32], vec![], std::mem::take(&mut a.c)) }
        // a panic trap stops the module (the host's absolute-address trap table does not apply here)
        "__exr_trap_at" => { a.o(&[0x00]); (vec![T_I64], vec![], vec![], std::mem::take(&mut a.c)) }
        // memmem(h, hl, n, nl) -> h+i or 0 (empty needle -> h). locals: 4 h32, 5 hl32, 6 n32, 7 nl32, 8 i, 9 j, 10 result
        "memmem" => {
            let c = vec![
                0x20, 0x00, 0xA7, 0x21, 0x04, 0x20, 0x01, 0xA7, 0x21, 0x05, 0x20, 0x02, 0xA7, 0x21, 0x06, 0x20, 0x03, 0xA7, 0x21, 0x07,
                0x42, 0x00, 0x21, 0x0A, 0x41, 0x00, 0x21, 0x08, 0x02, 0x40, 0x03, 0x40,
                0x20, 0x0A, 0x50, 0x45, 0x0D, 0x01,
                0x20, 0x05, 0x20, 0x07, 0x49, 0x20, 0x08, 0x20, 0x05, 0x20, 0x07, 0x6B, 0x4B, 0x72, 0x0D, 0x01,
                0x41, 0x00, 0x21, 0x09, 0x02, 0x40, 0x03, 0x40,
                0x20, 0x09, 0x20, 0x07, 0x46, 0x04, 0x40, 0x20, 0x04, 0x20, 0x08, 0x6A, 0xAD, 0x21, 0x0A, 0x0C, 0x02, 0x0B,
                0x20, 0x04, 0x20, 0x08, 0x6A, 0x20, 0x09, 0x6A, 0x2D, 0x00, 0x00,
                0x20, 0x06, 0x20, 0x09, 0x6A, 0x2D, 0x00, 0x00, 0x47, 0x0D, 0x01,
                0x20, 0x09, 0x41, 0x01, 0x6A, 0x21, 0x09, 0x0C, 0x00, 0x0B, 0x0B,
                0x20, 0x08, 0x41, 0x01, 0x6A, 0x21, 0x08, 0x0C, 0x00, 0x0B, 0x0B, 0x20, 0x0A,
            ];
            (vec![T_I64, T_I64, T_I64, T_I64], vec![T_I64], vec![(6, T_I32), (1, T_I64)], c)
        }
        // round(x): half away from zero (wasm `nearest` rounds halves to even). t = trunc(x); |x-t| >= 0.5 ? t+copysign(1,x) : t
        "round" => {
            let c = vec![
                0x20, 0x00, 0x20, 0x00, 0x9D, 0x22, 0x01, 0xA1, 0x99,
                0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xE0, 0x3F, 0x66, 0x04, T_F64,
                0x20, 0x01, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, 0x20, 0x00, 0xA6, 0xA0,
                0x05, 0x20, 0x01, 0x0B,
            ];
            (vec![T_F64], vec![T_F64], vec![(1, T_F64)], c)
        }
        "roundf" => {
            let c = vec![
                0x20, 0x00, 0x20, 0x00, 0x8F, 0x22, 0x01, 0x93, 0x8B,
                0x43, 0x00, 0x00, 0x00, 0x3F, 0x60, 0x04, T_F32,
                0x20, 0x01, 0x43, 0x00, 0x00, 0x80, 0x3F, 0x20, 0x00, 0x98, 0x92,
                0x05, 0x20, 0x01, 0x0B,
            ];
            (vec![T_F32], vec![T_F32], vec![(1, T_F32)], c)
        }
        _ => unreachable!("wasi_shim_body: {name}"),
    };
    let mut body = vec![];
    if locals.is_empty() {
        uleb(&mut body, 0);
    } else {
        uleb(&mut body, locals.len() as u64);
        for (n, t) in &locals {
            uleb(&mut body, *n as u64);
            body.push(*t);
        }
    }
    body.extend_from_slice(&code);
    body.push(0x0B);
    (params, rets, body)
}

/// Writes a wasm32-unknown-unknown module (see [`write_module_inner`])
pub fn write_module(funcs: &[FuncDef], datas: &[DataDef]) -> Result<Vec<u8>, String> {
    write_module_inner(funcs, datas, WasmTarget::Unknown)
}

/// Writes a wasm32-wasip1 module: WASI preview1 imports and a synthesized `_start`
pub fn write_module_wasip1(funcs: &[FuncDef], datas: &[DataDef]) -> Result<Vec<u8>, String> {
    write_module_inner(funcs, datas, WasmTarget::WasiP1)
}

/// Writes a wasm32-wasip2 module (currently the same preview1-compatible core module as wasip1)
pub fn write_module_wasip2(funcs: &[FuncDef], datas: &[DataDef]) -> Result<Vec<u8>, String> {
    write_module_inner(funcs, datas, WasmTarget::WasiP2)
}

/// Writes the module: imports (module `env`, or `wasi_snapshot_preview1` for a WASI target) for what is called but
/// not defined, the functions, one table for the functions whose address is taken (index 0 is null), the memory
/// (stack first, then data, then heap), the stack pointer and heap top globals, and exports (`memory` and the
/// exported functions; a WASI target exports `memory` and a synthesized `_start` that calls `main` then `proc_exit`)
fn write_module_inner(funcs: &[FuncDef], datas: &[DataDef], target: WasmTarget) -> Result<Vec<u8>, String> {
    use std::collections::HashMap;
    let wasi = target != WasmTarget::Unknown;
    let mut fidx: HashMap<String, u32> = HashMap::new();
    let mut data_addr: HashMap<String, u64> = HashMap::new();
    // data layout: after the null page and the stack
    let mut addr = NULL_PAGE + STACK_SIZE;
    for d in datas {
        addr = addr.div_ceil(16) * 16;
        data_addr.insert(d.0.clone(), addr);
        addr += d.1.len().max(1) as u64;
    }
    let free_base = addr.div_ceil(16) * 16;
    let after_msg = (free_base as u64 + 64 * 8 + 16 + ERR_TEXT.len() as u64).div_ceil(16) * 16;
    // (WASI: a 32-byte iovec scratch after strerror's text; unknown-unknown keeps the original layout)
    let scratch = after_msg as i64;
    let heap_base = if wasi { (after_msg + 32).div_ceil(16) * 16 } else { after_msg };
    let lay = Layout { free_base: free_base as i64, errno: free_base as i64 + 64 * 8, msg: free_base as i64 + 64 * 8 + 16, scratch };
    // imports: called names that are neither defined nor built in, in order of first use
    let defined: std::collections::HashSet<&str> = funcs.iter().map(|f| f.0.as_str()).collect();
    let mut imports: Vec<(String, i64)> = vec![];
    let mut builtins: Vec<&str> = vec![];
    let need_builtin = |n: &str, builtins: &mut Vec<&str>| {
        let canon = BUILTINS.iter().find(|b| **b == n).copied().or_else(|| if wasi { WASI_SHIMS.iter().find(|b| **b == n).copied() } else { None });
        if let Some(b) = canon {
            if !builtins.contains(&b) {
                builtins.push(b);
            }
            // (realloc and calloc call malloc and free)
            if matches!(n, "realloc" | "calloc") {
                for d in ["malloc", "free"] {
                    if !builtins.contains(&d) {
                        builtins.push(d);
                    }
                }
            }
            true
        } else {
            false
        }
    };
    for f in funcs {
        for r in &f.2 {
            if r.kind == R_CALL && !defined.contains(r.name.as_str()) && !need_builtin(&r.name, &mut builtins) {
                match imports.iter().find(|i| i.0 == r.name) {
                    Some(i) if i.1 != r.addend => {
                        let (a, b) = (sig_dec(i.1), sig_dec(r.addend));
                        return Err(format!("wasm32: `{}` is called with two signatures ({a:?} and {b:?})", r.name));
                    }
                    Some(_) => {}
                    None => imports.push((r.name.clone(), r.addend)),
                }
            }
        }
    }
    // (addresses of functions that are only imported or built in)
    for f in funcs {
        for r in &f.2 {
            if r.kind == R_ADDR && !defined.contains(r.name.as_str()) && !data_addr.contains_key(&r.name) && !imports.iter().any(|i| i.0 == r.name) {
                need_builtin(&r.name, &mut builtins);
            }
        }
    }
    // (WASI: the preview1 imports the shims and _start call)
    let mut wasi_imports: Vec<(String, i64)> = vec![];
    if wasi {
        if builtins.iter().any(|b| *b == "putchar" || *b == "write") {
            wasi_imports.push(("fd_write".into(), sig_enc(&[T_I32, T_I32, T_I32, T_I32], &[T_I32])?));
        }
        // (_start always ends in proc_exit)
        wasi_imports.push(("proc_exit".into(), sig_enc(&[T_I32], &[])?));
    }
    let ni = imports.len() as u32;
    let nw = wasi_imports.len() as u32;
    for (i, im) in imports.iter().enumerate() {
        fidx.insert(im.0.clone(), i as u32);
    }
    for (i, im) in wasi_imports.iter().enumerate() {
        fidx.insert(im.0.clone(), ni + i as u32);
    }
    let func_base = ni + nw;
    for (i, f) in funcs.iter().enumerate() {
        fidx.insert(f.0.clone(), func_base + i as u32);
    }
    let builtin_base = func_base + funcs.len() as u32;
    for (i, b) in builtins.iter().enumerate() {
        fidx.entry(b.to_string()).or_insert(builtin_base + i as u32);
    }
    let start_idx = builtin_base + builtins.len() as u32;
    if wasi {
        fidx.insert("_start".into(), start_idx);
    }
    // types
    let mut types: Vec<i64> = vec![];
    let type_of = |s: i64, types: &mut Vec<i64>| -> u32 {
        match types.iter().position(|&t| t == s) {
            Some(i) => i as u32,
            None => {
                types.push(s);
                types.len() as u32 - 1
            }
        }
    };
    let mut func_types = vec![];
    let mut bodies: Vec<Vec<u8>> = vec![];
    for im in &imports {
        type_of(im.1, &mut types);
    }
    let import_types: Vec<u32> = imports.iter().map(|im| type_of(im.1, &mut types)).collect();
    let wasi_import_types: Vec<u32> = wasi_imports.iter().map(|im| type_of(im.1, &mut types)).collect();
    // the table: functions whose address is taken (from code and from data), index 0 is null
    let mut table: Vec<u32> = vec![];
    let mut table_idx: HashMap<String, u32> = HashMap::new();
    let elem = |n: &str, table: &mut Vec<u32>, table_idx: &mut HashMap<String, u32>| -> Result<i64, String> {
        if let Some(&t) = table_idx.get(n) {
            return Ok(t as i64);
        }
        let f = *fidx.get(n).ok_or_else(|| format!("wasm32: undefined symbol `{n}` (not available on this wasm target)"))?;
        table.push(f);
        let t = table.len() as u32;
        table_idx.insert(n.to_string(), t);
        Ok(t as i64)
    };
    for f in funcs {
        let code = &f.1;
        if code.len() < 8 {
            return Err(format!("wasm32: `{}` was not compiled for wasm32", f.0));
        }
        let sig = i64::from_le_bytes(code[..8].try_into().unwrap());
        func_types.push(type_of(sig, &mut types));
        let mut body = code[8..].to_vec();
        for r in &f.2 {
            let at = r.offset as usize - 8;
            match r.kind {
                R_CALL => {
                    let i = *fidx.get(&r.name).ok_or_else(|| format!("wasm32: undefined function `{}`", r.name))?;
                    uleb5_at(&mut body[at..at + 5], i);
                }
                R_TYPE => {
                    let t = type_of(r.addend, &mut types);
                    uleb5_at(&mut body[at..at + 5], t);
                }
                R_ADDR => {
                    let v = match data_addr.get(&r.name) {
                        Some(&a) => a as i64 + r.addend,
                        None => elem(&r.name, &mut table, &mut table_idx)?,
                    };
                    sleb10_at(&mut body[at..at + 10], v);
                }
                k => return Err(format!("wasm32: relocation kind {k} in `{}`", f.0)),
            }
        }
        bodies.push(body);
    }
    for b in &builtins {
        let (p, r, body) = if BUILTINS.iter().any(|x| x == b) { builtin_body(b, &lay, &|n| fidx[n]) } else { wasi_shim_body(b, &lay, &|n| fidx[n]) };
        let sig = sig_enc(&p, &r)?;
        // (a call with another signature would not validate: say which)
        for f in funcs {
            if let Some(r) = f.2.iter().find(|r| r.kind == R_CALL && r.name == *b && r.addend != sig) {
                return Err(format!("wasm32: `{b}` is called as {:?}, but the built-in one is {:?}", sig_dec(r.addend), sig_dec(sig)));
            }
        }
        func_types.push(type_of(sig, &mut types));
        bodies.push(body);
    }
    // (WASI: _start calls main, then proc_exit with main's exit code)
    if wasi {
        let main_f = funcs.iter().find(|f| f.0 == "main").ok_or("wasm32-wasi: a WASI target needs `main`")?;
        let main_idx = fidx["main"];
        let exit_idx = fidx["proc_exit"];
        let main_sig = i64::from_le_bytes(main_f.1[..8].try_into().unwrap());
        let (_, main_rets) = sig_dec(main_sig);
        let mut a = Asm { c: vec![] };
        a.call(main_idx);
        if main_rets.is_empty() {
            a.o(&[0x41, 0x00]); // main returned nothing: exit 0
        }
        a.call(exit_idx);
        let mut body = vec![0x00]; // no locals
        body.extend_from_slice(&a.c);
        body.push(0x0B);
        func_types.push(type_of(sig_enc(&[], &[])?, &mut types));
        bodies.push(body);
    }
    // data bytes with their relocations (8-byte addresses / table indices)
    let mut segs: Vec<(u64, Vec<u8>)> = vec![];
    for d in datas {
        let mut bytes = d.1.clone();
        for (off, n, add) in &d.2 {
            let v = match data_addr.get(n) {
                Some(&a) => a as i64 + add,
                None => elem(n, &mut table, &mut table_idx)?,
            };
            let o = *off as usize;
            if o + 8 <= bytes.len() {
                bytes[o..o + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        if !bytes.iter().all(|&x| x == 0) {
            segs.push((data_addr[&d.0], bytes));
        }
    }
    if builtins.contains(&"strerror") {
        segs.push((lay.msg as u64, ERR_TEXT.to_vec()));
    }
    // --- sections ---
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    let mut s = vec![];
    uleb(&mut s, types.len() as u64);
    for &t in &types {
        let (p, r) = sig_dec(t);
        s.push(0x60);
        uleb(&mut s, p.len() as u64);
        s.extend_from_slice(&p);
        uleb(&mut s, r.len() as u64);
        s.extend_from_slice(&r);
    }
    section(&mut m, 1, &s);
    let mut s = vec![];
    uleb(&mut s, (imports.len() + wasi_imports.len()) as u64);
    for (im, &t) in imports.iter().zip(&import_types) {
        name_bytes(&mut s, "env");
        name_bytes(&mut s, &im.0);
        s.push(0x00);
        uleb(&mut s, t as u64);
    }
    for (im, &t) in wasi_imports.iter().zip(&wasi_import_types) {
        name_bytes(&mut s, "wasi_snapshot_preview1");
        name_bytes(&mut s, &im.0);
        s.push(0x00);
        uleb(&mut s, t as u64);
    }
    section(&mut m, 2, &s);
    let mut s = vec![];
    uleb(&mut s, func_types.len() as u64);
    for &t in &func_types {
        uleb(&mut s, t as u64);
    }
    section(&mut m, 3, &s);
    // table (funcref, min = max = entries + 1)
    let mut s = vec![];
    uleb(&mut s, 1);
    s.push(0x70);
    s.push(0x01);
    uleb(&mut s, table.len() as u64 + 1);
    uleb(&mut s, table.len() as u64 + 1);
    section(&mut m, 4, &s);
    // memory
    let pages = (heap_base + 65535) / 65536 + 1;
    let mut s = vec![];
    uleb(&mut s, 1);
    s.push(0x01);
    uleb(&mut s, pages);
    uleb(&mut s, MAX_PAGES);
    section(&mut m, 5, &s);
    // globals: stack pointer, heap top (mutable i64)
    let mut s = vec![];
    uleb(&mut s, 2);
    for v in [(NULL_PAGE + STACK_SIZE) as i64, heap_base as i64] {
        s.push(T_I64);
        s.push(0x01);
        s.push(0x42);
        sleb(&mut s, v);
        s.push(0x0B);
    }
    section(&mut m, 6, &s);
    // exports
    let mut ex: Vec<(String, u8, u32)> = vec![("memory".into(), 0x02, 0)];
    if wasi {
        // (a WASI command module: the host runs _start)
        ex.push(("_start".into(), 0x00, start_idx));
    } else {
        for f in funcs {
            if f.3 {
                ex.push((f.0.clone(), 0x00, fidx[&f.0]));
            }
        }
    }
    let mut s = vec![];
    uleb(&mut s, ex.len() as u64);
    for (n, k, i) in &ex {
        name_bytes(&mut s, n);
        s.push(*k);
        uleb(&mut s, *i as u64);
    }
    section(&mut m, 7, &s);
    // elements: the table from index 1
    if !table.is_empty() {
        let mut s = vec![];
        uleb(&mut s, 1);
        s.push(0x00);
        s.extend_from_slice(&[0x41, 0x01, 0x0B]);
        uleb(&mut s, table.len() as u64);
        for &f in &table {
            uleb(&mut s, f as u64);
        }
        section(&mut m, 9, &s);
    }
    // code
    let mut s = vec![];
    uleb(&mut s, bodies.len() as u64);
    for b in &bodies {
        uleb(&mut s, b.len() as u64);
        s.extend_from_slice(b);
    }
    section(&mut m, 10, &s);
    // data (active segments in memory 0)
    let mut s = vec![];
    uleb(&mut s, segs.len() as u64);
    for (a, bytes) in &segs {
        s.push(0x00);
        s.push(0x41);
        sleb(&mut s, *a as i32 as i64);
        s.push(0x0B);
        uleb(&mut s, bytes.len() as u64);
        s.extend_from_slice(bytes);
    }
    section(&mut m, 11, &s);
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_round_trip() {
        let p = vec![T_I64, T_I32, T_F64, T_F32, T_I64];
        let r = vec![T_I64, T_I64];
        let e = sig_enc(&p, &r).unwrap();
        assert_eq!(sig_dec(e), (p, r));
        assert_eq!(sig_dec(sig_enc(&[], &[]).unwrap()), (vec![], vec![]));
    }

    #[test]
    fn padded_lebs() {
        let mut b = [0u8; 5];
        uleb5_at(&mut b, 300);
        assert_eq!(b, [0xAC, 0x82, 0x80, 0x80, 0x00]);
        let mut c = [0u8; 10];
        sleb10_at(&mut c, -1);
        assert_eq!(c[9], 0x7F);
        assert!(c[..9].iter().all(|&x| x == 0xFF));
    }
}
