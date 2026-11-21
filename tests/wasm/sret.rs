// 構造体の値返し (sret) と Option::take / enum match を確認する。
// 以前はフレームの基底を old_SP で取っていたため、sret の書き戻し先がずれ、ネストした呼び出しでスタックが壊れていた
#![no_std]
#![no_main]
use core::panic::PanicInfo;
unsafe extern "C" { fn putchar(c: i32) -> i32; }
#[panic_handler]
fn panic(_i: &PanicInfo) -> ! { loop {} }
fn put(c: u8) { unsafe { putchar(c as i32); } }
fn print_i64(n: i64) {
    if n < 0 { put(b'-'); }
    let mut v = if n < 0 { (0 - n) as u64 } else { n as u64 };
    let mut buf = [0u8; 20];
    let mut i = 20;
    loop { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    for &b in &buf[i..] { put(b); }
    put(b'\n');
}
struct Stack { items: [Option<i64>; 8], len: usize }
impl Stack {
    fn new() -> Self { Stack { items: [None; 8], len: 0 } }
    fn push(&mut self, v: i64) { self.items[self.len] = Some(v); self.len += 1; }
    fn pop(&mut self) -> Option<i64> {
        if self.len == 0 { return None; }
        self.len -= 1;
        self.items[self.len].take()
    }
}
fn make_pair(a: i64, b: i64) -> (i64, i64) { (a + b, a * b) }
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut st = Stack::new();
    st.push(2);
    st.push(3);
    st.push(4);
    let b = st.pop().unwrap_or(-99); // 4
    let a = st.pop().unwrap_or(-99); // 3
    st.push(a * b); // 12
    let d = st.pop().unwrap_or(-99); // 12
    let c = st.pop().unwrap_or(-99); // 2
    print_i64(c + d); // 14
    print_i64(st.len as i64); // 0
    let e: Option<i64> = Some(42);
    match e { Some(x) => print_i64(x), None => print_i64(-1) } // 42
    let (s, p) = make_pair(3, 5); // 8, 15
    print_i64(s);
    print_i64(p);
    0
}
