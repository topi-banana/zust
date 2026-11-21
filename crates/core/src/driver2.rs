//! syn と Cranelift を使わないドライバ: ソース → (lexer / macros / prelude) → ast_summary → Program →
//! acg (自前の構文木のコード生成) → minclif の ELF の .o → リンク。
//! driver.rs と同じ手順を、外部クレートに依存しないモジュールだけで行う (zustc 自身でもコンパイルできる)。
//! キャッシュと並列化はまだない。

use std::sync::Mutex;

use crate::acg::{self, Blob, ItemFn};
use crate::key::{Key, KeyHasher};
use crate::fxhash::{FxMap, FxSet};
use crate::lexer::{self, ItemKind, SrcItem};
use crate::parse::{Block, Expr, Pat, Stmt};
use crate::front::{FrontMemo, with_prelude_in};
use crate::prof::{Timer, now_ns};
use crate::program::{Insts, Program, TyEnv};
use crate::types::{Summary, Ty};

pub type R<T> = Result<T, String>;

/// ビルド (機械語・オブジェクト・リンク・ワークスペース)。feature `build` (既定) のときだけ
#[cfg(feature = "build")]
mod build;
#[cfg(feature = "build")]
pub use build::*;

/// check の記録 (LSP の編集をまたいで): 宣言の指紋と、検査に通った関数・インスタンス (名前 → 記録)
pub struct ChkMemo {
    fp: u64,
    ents: FxMap<String, ChkEnt>,
}

#[derive(Clone)]
struct ChkEnt {
    /// 本体の関数の名前と、そのアイテムのハッシュ
    body: String,
    body_hash: Key,
    /// 要求したインスタンスの名前
    children: Vec<String>,
    /// インスタンスなら、自分を要求し直すための Blob (自分への再配置だけ)
    me: Vec<u8>,
    /// 検査に通ったか (通らなかったものは毎回検査し直す)
    ok: bool,
}


/// 解析の結果
pub struct Analysis {
    /// 宣言の指紋: fn アイテムは要約 (シグネチャ・クロージャ)、ほかのアイテムはテキストのハッシュを並べたもの。
    /// fn の本体だけを変えた編集では変わらない (check の記憶 ChkMemo を使ってよいかの印)
    pub iface_fp: u64,
    pub src: String,
    pub items: Vec<SrcItem>,
    pub prog: Program,
    pub refs: Vec<Vec<u32>>,
    /// 関数ごとの、名前で現れる struct と const / static (キャッシュのキー用)
    pub srefs: Vec<Vec<u32>>,
    pub crefs: Vec<Vec<u32>>,
    /// 要約を作り直した (キャッシュになかった) アイテムの数
    pub n_parsed: usize,
    /// アイテムごとの、依存するクレートのものか (CrateCtx)
    pub ext_items: Vec<bool>,
    /// `-O` で展開してよい (prelude の小さい) 関数の本体
    pub inl: acg::Inline,
    /// 機械語にするバックエンド (None なら minclif)
    pub cl: Option<Backend>,
    /// prelude の先頭の行 (結合したソースの行)
    pub prelude_line0: usize,
    /// 読んだ関数の本体 (関数 id → 構文木)
    pub asts: Mutex<FxMap<u32, ItemFn>>,
    /// 囲む関数 (アイテム番号) ごとのクロージャの式 (クロージャの本体を引くたびに囲む関数を読み直さない)
    /// item → its closures, and whether each is written inside `unsafe { }`
    pub encl: Mutex<FxMap<u32, Vec<(Expr, bool)>>>,
    /// 関数ごとのシグネチャのハッシュ (0 はまだ: キーを作るたびに参照先のシグネチャをたどり直さない)
    pub sig_keys: Mutex<Vec<Key>>,
    /// 依存するクレートのテキストの範囲と、そこから見えるクレート (CrateCtx::dep_ranges)
    pub dep_ranges: Vec<(u32, u32, Vec<usize>, u32)>,
}

/// クレートの印 (panic の場所の行の上位ビット。ライブラリの panic の表の 8 byte 目): 1..=4000
/// `L:C: ..` with L a tagged line (`tag << 20 | line in the crate`, line_base_for): the line in the combined source
/// (the `(at L:C)` of a second place too)
fn untag_lines(e: &str, shift: u32) -> String {
    let fix = |n: u64| if n >= 1 << 20 { (n & ((1 << 20) - 1)) + shift as u64 } else { n };
    let mut out = String::with_capacity(e.len());
    let b = e.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // (a number followed by `:` and a digit, at the start or after `at ` / ` (at `)
        let at_start = i == 0 || e[..i].ends_with("at ");
        if at_start && b[i].is_ascii_digit() {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < b.len() && b[j] == b':' && b[j + 1].is_ascii_digit() {
                let n: u64 = e[i..j].parse().unwrap_or(0);
                out.push_str(&fix(n).to_string());
                i = j;
                continue;
            }
        }
        let ch = e[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub fn crate_tag(name: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    1 + h % 4000
}

/// prelude の中の panic の場所の行の基準 (結合したソースの行によらない: prelude の関数の機械語をクレートの間で使い回す)。
/// 場所の表では、この行から後ろは prelude のファイル
pub const PRELUDE_LINE: u32 = 49152;

/// 検査だけの印のキャッシュのキー (ビルドの Blob のキーと混ざらないように)
const CHECK_SALT: Key = 0x5eed_c4ec_0000_0000_0000_0000_0000_0001;

impl Analysis {
    /// アイテム ii が依存するクレートのものなら、その dep_ranges の番号
    /// シグネチャのハッシュ (覚えておく)
    pub fn sig_key(&self, id: u32) -> Key {
        let mut ks = self.sig_keys.lock().unwrap();
        if ks.is_empty() {
            ks.resize(self.prog.names.len(), 0);
        }
        let k = ks[id as usize];
        if k != 0 {
            return k;
        }
        let mut h = KeyHasher::new();
        self.prog.hash_sig(id, &mut h);
        let k = h.finish() | 1;
        ks[id as usize] = k;
        k
    }

    pub fn crate_of_item(&self, ii: usize) -> Option<usize> {
        if ii >= self.prog.prelude_from as usize || !self.ext_items.get(ii).copied().unwrap_or(false) {
            return None;
        }
        let l = self.items[ii].line as u32;
        self.dep_ranges.iter().position(|r| r.0 <= l && l < r.1)
    }

    /// 閉じた関数 (インスタンス): 本体が prelude か依存するクレートにあり、型引数にこのクレートの型を含まないもの。
    /// 本体が呼びうるのは、本体のクレートと型引数の型のクレートから見えるクレート (と prelude) のものだけなので、
    /// キャッシュのキーはそれらだけで決まる (使う側によらない)。返り値は見えるクレート (dep_ranges の番号ごと)
    pub fn closed_univ(&self, insts: &Insts, id: u32) -> Option<Vec<bool>> {
        let body = insts.body_of(id);
        if self.prog.closures.contains_key(&body) {
            return None;
        }
        let ii = self.prog.item_of[body as usize]? as usize;
        let mut u = vec![false; self.dep_ranges.len()];
        if ii < self.prog.prelude_from as usize {
            let r = self.crate_of_item(ii)?;
            for &x in &self.dep_ranges[r].2 {
                u[x] = true;
            }
        }
        if let Some(inst) = insts.get(id) {
            let mut stack: Vec<Ty> = inst.args.clone();
            while let Some(t) = stack.pop() {
                match t {
                    Ty::Ref(_, i) | Ty::Ptr(_, i) | Ty::Array(i, _) | Ty::Slice(i) | Ty::Box(i) => stack.push(*i),
                    Ty::Struct(sid) => {
                        let b = self.prog.base_of(sid);
                        if let Some(&sii) = self.prog.struct_item.get(b as usize) {
                            let sii = sii as usize;
                            if sii < self.prog.prelude_from as usize {
                                let c = self.crate_of_item(sii)?;
                                for &x in &self.dep_ranges[c].2 {
                                    u[x] = true;
                                }
                            }
                        }
                        stack.extend(self.prog.structs[sid as usize].args.iter().copied());
                    }
                    Ty::Fn(f) => {
                        stack.extend(f.params.iter().copied());
                        stack.push(f.ret);
                    }
                    Ty::Dyn(_) | Ty::Assoc(_) | Ty::Param(_) => return None,
                    _ => {}
                }
            }
        }
        Some(u)
    }

    /// 関数 (インスタンス) id の panic の場所の行の基準。閉じた依存するクレートの関数は、そのクレートのテキストの先頭からの
    /// 行 (使う側の結合したソースの中の位置によらない: .o を書くときに位置を足す)
    pub fn line_base_for(&self, insts: &Insts, id: u32) -> Option<u32> {
        let body = insts.body_of(id);
        let ii = self.prog.item_of[body as usize]? as usize;
        if let Some(r) = self.crate_of_item(ii) {
            if self.closed_univ(insts, id).is_some() {
                return Some((self.dep_ranges[r].3 << 20) | (self.items[ii].line as u32 - self.dep_ranges[r].0));
            }
        }
        Some(self.line_base(ii))
    }

    /// .o に書くときに、関数 id の panic の場所の行に足す数 (line_base_for で依存するクレートの中の行にしたもの)
    pub fn line_shift(&self, insts: &Insts, id: u32) -> u32 {
        let body = insts.body_of(id);
        let Some(ii) = self.prog.item_of.get(body as usize).copied().flatten() else { return 0 };
        match self.crate_of_item(ii as usize) {
            Some(r) if self.closed_univ(insts, id).is_some() => self.dep_ranges[r].0 - 1,
            _ => 0,
        }
    }

    /// アイテム ii の panic の場所の行の基準 (アイテムの先頭の行 - 1)
    pub fn line_base(&self, ii: usize) -> u32 {
        let it = &self.items[ii];
        let user_lines = self.prelude_line0 as u32;
        if ii >= self.prog.prelude_from as usize && self.prelude_line0 > 0 && user_lines < PRELUDE_LINE && it.line >= self.prelude_line0 {
            PRELUDE_LINE + (it.line - self.prelude_line0).saturating_sub(1) as u32
        } else {
            it.line as u32 - 1
        }
    }
}

impl Analysis {
    /// 依存するクレートの .o にある関数 (ここではコンパイルしない)
    pub fn is_ext(&self, id: u32) -> bool {
        concrete(&self.prog, id as usize) && self.prog.item_of[id as usize].is_some_and(|ii| self.ext_items[ii as usize])
    }
    /// 依存するクレートの static
    pub fn is_ext_static(&self, ci: u32) -> bool {
        self.ext_items.get(self.prog.consts[ci as usize].item as usize).copied().unwrap_or(false)
    }
}

pub fn analyze(src0: &str, opt: bool) -> R<Analysis> {
    Session2::new(Options2 { opt, no_cache: true, jobs: 1, ..Default::default() }).analyze(src0)
}

/// 機械語にするバックエンド (Cranelift: zust-clif の compile)。feature `build` なしでは使わない
#[cfg(feature = "build")]
pub type Backend = crate::lower::ClBackend;
#[cfg(not(feature = "build"))]
pub type Backend = fn();

/// ドライバの設定
#[derive(Clone, Debug)]
pub struct Options2 {
    pub opt: bool,
    /// 関数のコンパイルに使うプロセスの数 (fork する)
    pub jobs: usize,
    pub linker: Option<String>,
    /// `-L` directories, searched for the `lib<name>.a` of a `#[link(name = "..")]`
    pub lib_dirs: Vec<std::path::PathBuf>,
    pub no_cache: bool,
    /// 段階ごとの時間を stderr に出す
    pub time: bool,
    /// minclif の代わりのバックエンド (Cranelift: crate::clif_backend::compile)
    pub backend: Option<Backend>,
    /// `#[cfg(feature = "..")]` で真になる feature (1 つのクレートのビルド。ワークスペースはクレートごとに CrateSpec が持つ)
    pub features: Vec<String>,
    /// wasm32-unknown-unknown: writes the module instead of an ELF object, and does not link (zust_wasm::write_module;
    /// `backend` is then zust_wasm::compile)
    pub module_writer: Option<ModuleWriter>,
    /// the wasm target is wasm32-wasip1 / wasip2 (WASI): `cfg(target_os = "wasi")`, and the writer synthesizes `_start`
    pub wasi: bool,
}

/// Writes the output from the functions and data build.rs lays out (in place of the ELF object)
pub type ModuleWriter = fn(&[(String, Vec<u8>, Vec<crate::ir::Reloc>, bool)], &[(String, Vec<u8>, Vec<(u32, String, i64)>, bool)]) -> Result<Vec<u8>, String>;

impl Default for Options2 {
    fn default() -> Self {
        Options2 { opt: false, jobs: cpu_count(), linker: None, lib_dirs: vec![], no_cache: false, time: false, backend: None, features: vec![], module_writer: None, wasi: false }
    }
}

impl Options2 {
    /// キャッシュのキーに入れるバックエンドの印 (Cranelift なら 1)
    pub fn backend_id(&self) -> u8 {
        // (wasm: the address checks are part of the code, so a build without them (ZUSTC_WASM_NO_ADDR_CHECK, for
        // measuring) has its own cache entries)
        if self.module_writer.is_some() {
            return if std::env::var_os("ZUSTC_WASM_NO_ADDR_CHECK").is_some() { 3 } else { 2 };
        }
        self.backend.is_some() as u8
    }

    /// cfg の述語の設定 (features は与えたもの)
    pub fn cfg_with(&self, features: &[String]) -> crate::cfg::CfgSet {
        crate::cfg::CfgSet { features: features.to_vec(), debug_assertions: !self.opt, wasm: self.module_writer.is_some(), wasi: self.wasi }
    }
}

/// 複数のクレートのうちの 1 つをコンパイルするときの情報。ソースは「このクレートのコード (1..=own_lines 行)」の後ろに
/// 依存するクレートを `pub mod name { .. }` として付けたもの。依存するクレートの (総称でない) 関数と static は
/// そのクレートの .o にあるので、ここではコンパイルせずシンボルで参照する
#[derive(Clone, Debug, Default)]
pub struct CrateCtx {
    /// ライブラリをコンパイルするときはその名前 (総称でない関数と static を外に見せる)
    pub lib: Option<String>,
    /// このクレートのコードの行数 (0 なら全部)
    pub own_lines: u32,
    /// 結合したソースの行 → ファイル
    pub map: crate::srcmap::SrcMap,
    /// 依存するクレート (推移的に全部。実行ファイルの panic の表がそれらの表を指す)
    pub deps: Vec<String>,
    /// このクレートの feature (ワークスペース。None なら Options2 の features)
    pub features: Option<Vec<String>>,
    /// 依存するクレートのテキストの範囲 (結合したソースの行 [開始, 終わり)) と、そこから見えるクレート (この表の番号。自分も)
    pub dep_ranges: Vec<(u32, u32, Vec<usize>, u32)>,
}

/// ビルドの結果
#[derive(Clone, Debug, Default)]
pub struct Report2 {
    pub items_parsed: usize,
    pub fns_compiled: usize,
    pub fns_hit: usize,
    pub object_bytes: usize,
    pub linked: bool,
}

// (read / write は std と同じ名前の libc の関数。ポインタを *mut u8 で受ける: zustc でもコンパイルできるように)
#[allow(suspicious_runtime_symbol_definitions)]
unsafe extern "C" {
    #[link_name = "sysconf"]
    fn c_sysconf(name: i32) -> i64;
    #[link_name = "stat"]
    fn c_stat(path: *const u8, buf: *mut u8) -> i32;
    #[link_name = "fork"]
    fn c_fork() -> i32;
    #[link_name = "pipe"]
    fn c_pipe(fds: *mut i32) -> i32;
    #[link_name = "read"]
    fn c_read(fd: i32, buf: *mut u8, n: usize) -> isize;
    #[link_name = "write"]
    fn c_write(fd: i32, buf: *const u8, n: usize) -> isize;
    #[link_name = "close"]
    fn c_close(fd: i32) -> i32;
    #[link_name = "waitpid"]
    fn c_waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    #[cfg(feature = "build")]
    #[link_name = "mkdir"]
    fn c_mkdir(path: *const u8, mode: u32) -> i32;
    #[link_name = "_exit"]
    fn c_exit(code: i32) -> !;
}

/// 使える CPU の数
pub fn cpu_count() -> usize {
    // SAFETY: _SC_NPROCESSORS_ONLN (84)
    let n = unsafe { c_sysconf(84) };
    if n < 1 { 1 } else { n as usize }
}

/// 単調時計 (ナノ秒)

/// コンパイラ自身の識別 (実行ファイルの大きさと更新時刻): 作り直したらキャッシュを使わない
fn compiler_id() -> Key {
    let mut buf = [0u8; 256];
    // SAFETY: "/proc/self/exe" は NUL で終わる。buf は struct stat (144 byte) より大きい
    let r = unsafe { c_stat(b"/proc/self/exe\0".as_ptr(), buf.as_mut_ptr()) };
    let mut h = KeyHasher::new();
    h.add("driver2-1");
    if r == 0 {
        // (x86-64: st_size は 48、st_mtim は 88 / 96)
        h.add(&buf[48..56]);
        h.add(&buf[88..104]);
    }
    h.finish()
}

/// 時間の計測 (-time)

/// キャッシュを持つドライバ (driver.rs の Session と同じ使い方)
pub struct Session2 {
    pub opts: Options2,
    cid: Key,
    disk: crate::cache2::Cache2,
    mem: FxMap<Key, Vec<u8>>,
    used: FxSet<Key>,
    /// put したキー (ワークスペースの子プロセスが、作ったエントリを親に返す)
    fresh: Vec<Key>,
    /// prelude の選択の、アイテムごとの記憶 (LSP の編集のたびに全アイテムを調べ直さない)
    sel: FrontMemo,
    /// fn アイテムのハッシュ → 要約の符号のハッシュ (Analysis::iface_fp を作るため)
    sum_fp: FxMap<Key, u64>,
    /// 前回の check の記録
    chk: Option<ChkMemo>,
    /// 複数のクレートのうちの 1 つをコンパイルするとき
    pub ctx: CrateCtx,
    /// the files the last build_path read (for the CLI's stamp file)
    pub last_map: Option<crate::srcmap::SrcMap>,
}

impl Session2 {
    pub fn new(opts: Options2) -> Session2 {
        Session2 { opts, cid: compiler_id(), disk: crate::cache2::Cache2::empty(), mem: FxMap::default(), used: FxSet::default(), fresh: vec![], ctx: CrateCtx::default(), last_map: None, sel: Default::default(), sum_fp: FxMap::default(), chk: None }
    }

    pub fn load_cache(&mut self, path: &std::path::Path) {
        if !self.opts.no_cache {
            self.disk = crate::cache2::Cache2::open(path);
        }
    }

    /// 今回使ったエントリだけを書く
    pub fn save_cache(&mut self, path: &std::path::Path) -> Result<usize, String> {
        // (今回使ったものはディスクの上の古い表にしかなくてもメモリに移す: 書いたファイルを開き直さないので)
        let used = self.used.clone();
        for &k in &used {
            if !self.mem.contains_key(&k) {
                if let Some(v) = self.disk.get(k) {
                    let v = v.to_vec();
                    self.mem.insert(k, v);
                }
            }
        }
        let n = self.save_cache_keep(path)?;
        self.mem.retain(|k, _| used.contains(k));
        self.disk = crate::cache2::Cache2::empty();
        Ok(n)
    }

    /// save_cache と同じだが、メモリの上のエントリは残す (ワークスペースで次のクレートが使う)
    fn save_cache_keep(&mut self, path: &std::path::Path) -> Result<usize, String> {
        if self.opts.no_cache {
            return Ok(0);
        }
        let mut keys: Vec<Key> = self.used.iter().copied().collect();
        keys.sort_unstable();
        let mut entries: Vec<(Key, Vec<u8>)> = vec![];
        for k in keys {
            let v = match self.mem.get(&k) {
                Some(v) => v.clone(),
                None => match self.disk.get(k) {
                    Some(v) => v.to_vec(),
                    None => continue,
                },
            };
            entries.push((k, v));
        }
        let refs: Vec<(Key, &[u8])> = entries.iter().map(|(k, v)| (*k, &v[..])).collect();
        let n = crate::cache2::Cache2::save(path, &refs)?;
        self.used.clear();
        Ok(n)
    }

    fn get(&mut self, k: Key) -> Option<Vec<u8>> {
        if self.opts.no_cache {
            return None;
        }
        self.used.insert(k);
        match self.mem.get(&k) {
            Some(v) => Some(v.clone()),
            None => self.disk.get(k).map(|v| v.to_vec()),
        }
    }

    fn put(&mut self, k: Key, v: Vec<u8>) {
        if self.opts.no_cache {
            return;
        }
        self.used.insert(k);
        self.mem.insert(k, v);
        self.fresh.push(k);
    }

    /// 要約はアイテムのハッシュで引く
    pub fn analyze(&mut self, src0: &str) -> R<Analysis> {
        // (ファイルから読んだもの (load_crate) は読むときに cfg を評価してある)
        let stripped;
        let src0 = if self.ctx.map.inputs.is_empty() {
            match crate::cfg::strip(src0, &self.opts.cfg_with(&self.opts.features)) {
                Some(t) => {
                    stripped = t;
                    stripped.as_str()
                }
                None => src0,
            }
        } else {
            src0
        };
        let mut crates = self.ctx.deps.clone();
        crates.extend(self.ctx.lib.iter().cloned());
        let mut fix = vec![];
        let cfg = self.opts.cfg_with(self.ctx.features.as_ref().unwrap_or(&self.opts.features));
        let (src, items, user, prelude_line0) = with_prelude_in(src0, &crates, &mut fix, Some(&cfg), &mut self.sel)?;
        self.ctx.map.colfix = fix;
        // (E0432: a top-level `use` of a path whose first segment names nothing; the `use` items come from the item
        // split, the names they could mean from the original text)
        if self.ctx.map.inputs.is_empty() {
            if let Some(e) = unresolved_import(src0, &src, &items[..user], &self.ctx.deps) {
                return Err(e);
            }
            if let Some(e) = duplicate_import(src0, &src, &items[..user]) {
                return Err(e);
            }
            if let Some(e) = malformed_attr(src0, &src, &items[..user], self.ctx.deps.is_empty() && self.ctx.lib.is_none()) {
                return Err(e);
            }
        }
        let mut tm = Timer { on: std::env::var_os("ZUSTC_PROF").is_some(), last: now_ns(), start: now_ns() };
        if let Some(p) = std::env::var_os("ZUSTC_DUMP_SRC") {
            let _ = std::fs::write(p, &src);
        }
        let cid = self.cid;
        let mut sums: Vec<Summary> = vec![Summary::default(); items.len()];
        let mut n_parsed = 0;
        for pass in 0..2 {
            // (キャッシュにないもの: (番号, キー))
            let mut miss: Vec<(usize, Key)> = vec![];
            for (i, it) in items.iter().enumerate() {
                let header = matches!(it.kind, ItemKind::ImplHeader | ItemKind::TraitHeader);
                if header != (pass == 0) {
                    continue;
                }
                let mut h = KeyHasher::new();
                h.add(&cid);
                h.add(&0u8);
                h.add(&it.hash);
                let k = h.finish();
                if let Some(sm) = self.get(k).and_then(|d| Summary::decode(&d)) {
                    sums[i] = sm;
                    continue;
                }
                miss.push((i, k));
            }
            // (多ければ fork して分けて作る: 子 k は j % jobs == k のものを作り、直列化して返す)
            let jobs = self.opts.jobs.min(miss.len() / 32).max(1);
            let mut got: Vec<Option<Summary>> = vec![None; miss.len()];
            if jobs > 1 {
                let mut kids: Vec<(i32, i32)> = vec![];
                for kk in 0..jobs {
                    let mut fds = [0i32; 2];
                    // SAFETY: fds は 2 つの int
                    if unsafe { c_pipe(fds.as_mut_ptr()) } != 0 {
                        continue;
                    }
                    let pid = unsafe { c_fork() };
                    if pid == 0 {
                        unsafe { c_close(fds[0]) };
                        let mut buf = vec![];
                        let mut j = kk;
                        while j < miss.len() {
                            let it = &items[miss[j].0];
                            let ow = if pass == 1 { crate::ast_summary::owner_of(it, &sums) } else { None };
                            // (失敗したものは送らない: 親が作り直してエラーを出す)
                            if let Ok(sm) = crate::ast_summary::parse_and_summarize(&src, it, ow.as_ref()) {
                                let mut e = vec![];
                                sm.encode(&mut e);
                                crate::cache2::put_uv(&mut buf, j as u64);
                                crate::cache2::put_uv(&mut buf, e.len() as u64);
                                buf.extend_from_slice(&e);
                            }
                            j += jobs;
                        }
                        write_all(fds[1], &buf);
                        unsafe { c_close(fds[1]) };
                        unsafe { c_exit(0) };
                    }
                    unsafe { c_close(fds[1]) };
                    if pid < 0 {
                        unsafe { c_close(fds[0]) };
                        continue;
                    }
                    kids.push((pid, fds[0]));
                }
                for (pid, fd) in kids {
                    let d = read_all(fd);
                    unsafe { c_close(fd) };
                    let mut st = 0i32;
                    unsafe { c_waitpid(pid, &mut st, 0) };
                    let mut r = crate::cache2::Rd { d: &d, pos: 0 };
                    while r.pos < d.len() {
                        let (Some(j), Some(n)) = (r.uv(), r.uv()) else { break };
                        let Some(body) = r.take(n as usize) else { break };
                        if let Some(slot) = got.get_mut(j as usize) {
                            *slot = Summary::decode(body);
                        }
                    }
                }
            }
            for (j, &(i, k)) in miss.iter().enumerate() {
                let it = &items[i];
                let sm = match got[j].take() {
                    Some(sm) => sm,
                    None => {
                        let ow = if pass == 1 { crate::ast_summary::owner_of(it, &sums) } else { None };
                        crate::ast_summary::parse_and_summarize(&src, it, ow.as_ref()).map_err(|e| match (e.strip_prefix('\u{2}'), it.kind) {
                            // (`\u{2}msg`: an impl member's error that is at its impl's header)
                            (Some(m), ItemKind::ImplMember(h)) => format!("{}:1: {m}", items[h as usize].line),
                            _ => relocate(e.trim_start_matches('\u{2}').to_string(), it),
                        })?
                    }
                };
                n_parsed += 1;
                let mut b = vec![];
                sm.encode(&mut b);
                sums[i] = sm;
                self.put(k, b);
            }
        }
        // 宣言の指紋 (要約の符号のハッシュはアイテムのハッシュで覚える)
        let iface_fp = {
            let mut h = KeyHasher::new();
            h.add(&self.ctx.dep_ranges.len());
            let mut new_fp: FxMap<Key, u64> = FxMap::default();
            for (i, it) in items.iter().enumerate() {
                let is_fn = !sums[i].fns.is_empty() && sums[i].structs.is_empty() && sums[i].enums.is_empty() && matches!(it.kind, ItemKind::Normal | ItemKind::ImplMember(_) | ItemKind::TraitMember(_));
                if !is_fn || self.opts.opt {
                    h.add(&it.hash);
                    continue;
                }
                let x = match self.sum_fp.get(&it.hash) {
                    Some(&x) => x,
                    None => {
                        let mut e = vec![];
                        sums[i].encode(&mut e);
                        let mut hh = KeyHasher::new();
                        hh.add(&e[..]);
                        hh.finish() as u64
                    }
                };
                new_fp.insert(it.hash, x);
                h.add(&x);
            }
            self.sum_fp = new_fp;
            h.finish() as u64
        };
        tm.lap("summaries", {
            if tm.on {
                let ol = self.ctx.own_lines as usize;
                let own = items[..user].iter().filter(|it| ol == 0 || it.line <= ol).count();
                format!("{} items: own {own}, deps {}, prelude {}", items.len(), user - own, items.len() - user)
            } else {
                String::new()
            }
        });
        // (struct / enum / union definitions of the program: unused parameters, union fields ..)
        for (it, sm) in items[..user].iter().zip(&sums[..user]) {
            if let Some(e) = &sm.adt_err {
                // (`\u{1}field\u{1}msg`: at the line of the field `field:`)
                if let Some((f, m)) = e.strip_prefix('\u{1}').and_then(|r| r.split_once('\u{1}')) {
                    let text = src.get(it.start..it.end).unwrap_or("");
                    // (a lifetime is found where it is written, a field by `name:`)
                    let pat = if f.starts_with('\'') || f == "&" { f.to_string() } else { format!("{f}:") };
                    let line = text.find(pat.as_str()).map(|i| it.line + text[..i].matches('\n').count()).unwrap_or(it.line);
                    return Err(format!("{line}:1: {m}"));
                }
                return Err(format!("{}:1: {e}", it.line));
            }
        }
        // (`#![crate_type = "lib"]`: a library, no `main` needed)
        let mut deny_unreachable = false;
        let mut deny_unused_vars = false;
        let mut deny_dead_code = false;
        let mut deny_dead_pub = false;
        let mut deny_unused_imports = false;
        let mut deny_unreachable_code = false;
        let mut deny_must_use = false;
        let mut deny_unused_alloc = false;
        let mut deny_trivial_casts = (false, false);
        // (non_snake_case, non_camel_case_types, non_upper_case_globals)
        let mut deny_style = [false; 3];
        let lib_attr = {
            let mut end = src.len().min(4096);
            while !src.is_char_boundary(end) {
                end -= 1;
            }
            let c: String = src[..end].chars().filter(|c| !c.is_whitespace()).collect();
            // (`#![deny(unreachable_patterns)]`: inner attributes come first in the file, so the head is enough)
            deny_unreachable = (c.contains("#![deny(") || c.contains("#![forbid(")) && c.contains("unreachable_patterns");
            // (`#![deny(unused_variables)]` / `deny(unused)` / `deny(warnings)`)
            // (only the lints inside `#![deny(..)]` / `#![forbid(..)]`, not an `#![allow(unused_variables)]`)
            let mut levels: Vec<&str> = vec![];
            for head in ["#![deny(", "#![forbid("] {
                let mut rest = c.as_str();
                while let Some(k) = rest.find(head) {
                    rest = &rest[k + head.len()..];
                    if let Some(e) = rest.find(')') {
                        levels.extend(rest[..e].split(','));
                    }
                }
            }
            deny_unused_vars = levels.iter().any(|l| matches!(*l, "unused_variables" | "unused" | "warnings")) && !c.contains("#![allow(unused");
            deny_dead_code = levels.iter().any(|l| matches!(*l, "dead_code" | "unused" | "warnings")) && !c.contains("#![allow(dead_code") && !c.contains("#![expect(dead_code") && !c.contains("#![allow(unused");
            deny_dead_pub = levels.contains(&"dead_code_pub_in_binary");
            deny_unused_imports = levels.iter().any(|l| matches!(*l, "unused_imports" | "unused" | "warnings")) && !c.contains("#![allow(unused");
            deny_unreachable_code = levels.iter().any(|l| matches!(*l, "unreachable_code" | "unused" | "warnings")) && !c.contains("#![allow(unreachable_code");
            deny_must_use = levels.iter().any(|l| matches!(*l, "unused_must_use" | "unused" | "warnings")) && !c.contains("#![allow(unused_must_use");
            // (an `#![allow(..)]` of the lint undoes a deny, as the levels apply in order; forbid cannot be undone)
            // (the last of them wins: `#![allow(x)]` then `#![deny(x)]` denies)
            let lint = |n: &str| {
                // (the last `#![a(..)]` naming n anywhere in its list: `#![allow(dead_code, n)]`)
                let last = |a: &str| {
                    let open = format!("#![{a}(");
                    c.match_indices(&open)
                        .filter(|(i, _)| {
                            let rest = &c[i + open.len()..];
                            let list = &rest[..rest.find(')').unwrap_or(rest.len())];
                            list.split(',').any(|x| x.trim() == n)
                        })
                        .map(|(i, _)| i)
                        .last()
                };
                levels.contains(&n) && (last("forbid").is_some() || last("allow") < last("deny"))
            };
            deny_trivial_casts = (lint("trivial_casts"), lint("trivial_numeric_casts"));
            deny_unused_alloc = lint("unused_allocation")
                || (!levels.contains(&"unused_allocation") && levels.iter().any(|l| matches!(*l, "unused" | "warnings")) && !c.contains("#![allow(unused"));
            for (k, n) in ["non_snake_case", "non_camel_case_types", "non_upper_case_globals"].iter().enumerate() {
                deny_style[k] = levels.iter().any(|l| l == n || *l == "nonstandard_style" || *l == "warnings") && !c.contains(&format!("#![allow({n}"));
            }
            ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"].iter().any(|k| c.contains(&format!("#![crate_type=\"{k}\"]")))
        };
        // (dead_code, in a binary: the first fn / struct / enum / const / static / trait of the program's whose name
        // appears nowhere but in its own definition and in `impl` headers)
        if deny_dead_code && self.ctx.lib.is_none() {
            // (a `pub` item counts as used, except in a binary under `deny(dead_code_pub_in_binary)`)
            if let Some((line, msg)) = first_dead_item(&src, &items[..user], deny_dead_pub && !lib_attr) {
                return Err(format!("{line}:1: {msg}"));
            }
        }
        // (unused_imports: a top-level `use` whose name nothing else mentions)
        if deny_unused_imports && self.ctx.lib.is_none() {
            if let Some((line, col, msg)) = first_unused_import(&src, &items[..user]) {
                return Err(format!("{line}:{col}: {msg}"));
            }
        }
        // (naming lints: the first item whose name breaks its case convention)
        if deny_style.iter().any(|&d| d) && self.ctx.lib.is_none() {
            if let Some((line, msg)) = first_bad_name(&src, &items[..user], deny_style) {
                return Err(format!("{line}:1: {msg}"));
            }
        }
        // (unreachable_code: a statement after one that diverges)
        if deny_unreachable_code && self.ctx.lib.is_none() {
            for it in &items[..user] {
                let text = src.get(it.start..it.end).unwrap_or("");
                if !text.contains("fn ") || text.contains("allow(unreachable") || text.contains("#[cfg") {
                    continue;
                }
                let Ok(Some(f)) = parse_fn(&src, it) else { continue };
                if let Some((p, what)) = first_unreachable(&f.block) {
                    if p.line > 0 {
                        return Err(format!("{}:{}: unreachable {what}", it.line + p.line as usize - 1, p.col + 1));
                    }
                }
            }
        }
        // (unused_variables: the first binding nothing reads, in the program's own fns)
        if deny_unused_vars {
            for it in &items[..user] {
                let text = src.get(it.start..it.end).unwrap_or("");
                if !text.contains("fn ") || text.contains("allow(unused") || text.contains("#[cfg") {
                    continue;
                }
                let Ok(Some(f)) = parse_fn(&src, it) else { continue };
                let params: Vec<String> = f.sig.params.iter().filter_map(|(p, _)| match p {
                    Pat::Ident(_, _, n, None) => Some(n.clone()),
                    _ => None,
                }).collect();
                if let Some((pos, n)) = crate::ast_closure::first_unused_binding(&params, &f.block) {
                    let (line, col) = match pos {
                        Some(p) if p.line > 0 => (it.line + p.line as usize - 1, p.col as usize + 1),
                        _ => (it.line, 1),
                    };
                    return Err(format!("{line}:{col}: unused variable: `{n}`"));
                }
            }
        }
        unsafe_impls(&src, &items[..user])?;
        let mut prog = Program::build_with(&items, &sums, self.ctx.lib.is_none() && !lib_attr, user).map_err(|e| name_line(&e, &src, &items))?;
        tm.lap("program", String::new());
        prog.prelude_from = user as u32;
        // (E0277: a static is reachable from every thread, so its type must be Sync)
        if let Some((ci, tn)) = crate::acg::static_not_sync(&prog) {
            let line = items.get(prog.consts[ci].item as usize).map_or(0, |it| it.line);
            return Err(format!("{line}:1: `{tn}` cannot be shared between threads safely: the type of a `static` must be Sync (E0277)"));
        }
        // (traits of one name in two modules: which one a method call means depends on where it is. Read from the
        // crate's source (its files joined, as the items' lines count); a build with dependency crates keeps the earlier
        // choice, the first impl)
        if self.ctx.deps.is_empty() && self.ctx.lib.is_none() {
            prog.trait_scopes = lexer::trait_scopes(src0);
            if !prog.trait_scopes.is_empty() {
                prog.item_lines = items.iter().map(|it| it.line as u32).collect();
            }
        }
        prog.overflow_checks = !self.opts.opt;
        prog.deny_unreachable = deny_unreachable;
        prog.deny_must_use = deny_must_use;
        prog.deny_unused_alloc = deny_unused_alloc;
        prog.deny_trivial_casts = deny_trivial_casts;
        if deny_must_use {
            for it in &items[..user] {
                let text = src.get(it.start..it.end).unwrap_or("");
                if let Some(k) = text.find("#[must_use") {
                    let after = &text[k..];
                    if let Some(f) = after.find("fn ") {
                        let name: String = after[f + 3..].trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                        if !name.is_empty() {
                            prog.must_use_fns.push(name);
                        }
                    }
                }
            }
        }
        prog.opt = self.opts.opt;
        resolve_opaque(&mut prog, &src, &items)?;
        let ol = self.ctx.own_lines as usize;
        let ext_items: Vec<bool> = items.iter().enumerate().map(|(i, it)| ol > 0 && i < user && it.line > ol).collect();
        // (ライブラリ: 自分の総称でない関数は全部外に見せる (使う側のクレートが呼ぶ))
        if self.ctx.lib.is_some() {
            for id in 0..prog.names.len() {
                if concrete(&prog, id) && prog.item_of[id].is_some_and(|ii| (ii as usize) < user && !ext_items[ii as usize]) {
                    prog.sigs[id].export = true;
                }
            }
        }
        let mut refs = Vec::with_capacity(prog.item_of.len());
        let mut srefs = Vec::with_capacity(prog.item_of.len());
        let mut crefs = Vec::with_capacity(prog.item_of.len());
        let names = prog.ref_names();
        for ii in &prog.item_of {
            let (f, st, c) = ii.map(|ii| prog.refs_in(&src, &items[ii as usize], Some(&names))).unwrap_or_default();
            refs.push(f);
            srefs.push(st);
            crefs.push(c);
        }
        tm.lap("refs", String::new());
        let mut a = Analysis { iface_fp, src, items, prog, refs, srefs, crefs, n_parsed, ext_items, inl: acg::Inline::default(), cl: self.opts.backend, prelude_line0, dep_ranges: self.ctx.dep_ranges.clone(), asts: Mutex::new(FxMap::default()), encl: Mutex::new(FxMap::default()), sig_keys: Mutex::new(Vec::new()) };
        if self.opts.opt && std::env::var_os("ZUSTC_NO_INLINE").is_none() {
            a.inl = inline_bodies(&a);
        }
        Ok(a)
    }
}

/// `-> impl Iterator<..>` の戻り値を本体の型推論で決める (front.rs の resolve_opaque と同じ)
/// a `_` left in a type (inference did not decide it)
fn has_hole_ty(t: Ty) -> bool {
    match t {
        Ty::Param(i) => i == crate::program::HOLE,
        Ty::Ref(_, i) | Ty::Ptr(_, i) | Ty::Array(i, _) | Ty::Slice(i) | Ty::Box(i) => has_hole_ty(*i),
        _ => false,
    }
}

/// the declared return type (`Result<(), impl Debug>`, its `impl` parts holes) with the holes of the inferred one
/// filled from it, where both have the same shape (inference may leave `Ok`'s type open: `Err(Target)`)
fn merge_declared(prog: &Program, decl: Ty, inf: Ty) -> Ty {
    let hole = |t: Ty| t == Ty::Param(crate::program::HOLE);
    match (decl, inf) {
        (d, i) if hole(i) && !hole(d) => d,
        (Ty::Struct(x), Ty::Struct(y)) if x != y && prog.base_of(x) == prog.base_of(y) => {
            let (ax, ay) = (prog.structs[x as usize].args.clone(), prog.structs[y as usize].args.clone());
            if ax.len() != ay.len() {
                return inf;
            }
            let args: Vec<Ty> = ax.iter().zip(&ay).map(|(&d, &i)| merge_declared(prog, d, i)).collect();
            prog.instantiate(prog.base_of(x), args).map(Ty::Struct).unwrap_or(inf)
        }
        (Ty::Ref(_, d), Ty::Ref(m, i)) => Ty::Ref(m, crate::types::intern(merge_declared(prog, *d, *i))),
        (Ty::Box(d), Ty::Box(i)) => Ty::Box(crate::types::intern(merge_declared(prog, *d, *i))),
        _ => inf,
    }
}

fn resolve_opaque(prog: &mut Program, src: &str, items: &[SrcItem]) -> R<()> {
    if prog.opaque.is_empty() {
        return Ok(());
    }
    let ids = prog.opaque.clone();
    let mut done = vec![false; ids.len()];
    for _ in 0..4 {
        let mut progress = false;
        for (k, &id) in ids.iter().enumerate() {
            if done[k] {
                continue;
            }
            let sig = prog.sigs[id as usize].clone();
            let Some(ii) = prog.item_of[id as usize] else { continue };
            let Some(f) = parse_fn(src, &items[ii as usize])? else { continue };
            let env = TyEnv { self_sid: sig.impl_of, params: vec![], self_ty: sig.self_ty };
            // (a generic fn: each instance's return type comes from its body with the instance's arguments, when
            // the instance is made (Insts::request); calls before that see a new type variable)
            if sig.is_generic() {
                // (`fn opaque<T>(t: T) -> impl Sized { opaque(Some(t)) }`: each instance's type is the next one's)
                let me = prog.names[id as usize].rsplit("::").next().unwrap_or("").to_string();
                if let Some(crate::parse::Stmt::Expr(crate::parse::Expr::Call(callee, _), false)) = f.block.stmts.last() {
                    if matches!(&**callee, crate::parse::Expr::Path(None, p) if p.segs.last().is_some_and(|s| s.name == me)) {
                        return Err(format!("{}:1: cannot resolve opaque type (`{me}` returns its own result for other arguments)", items[ii as usize].line));
                    }
                }
                prog.opaque_generic.insert(id, std::sync::Arc::new((f.sig.params.clone(), f.block.clone(), f.sig.recv.is_some())));
                prog.sigs[id as usize].ret = Ty::Param(crate::program::HOLE);
                done[k] = true;
                progress = true;
                continue;
            }
            // (`-> Result<(), impl Debug>`: the declared type with a hole for each `impl`, which the body fills)
            let decl = f.sig.ret.as_ref().filter(|t| !matches!(t, crate::parse::Ty::Impl(_))).and_then(|t| {
                fn holes(t: &crate::parse::Ty) -> crate::parse::Ty {
                    use crate::parse::{GArg, Ty as P};
                    match t {
                        P::Impl(_) => P::Infer,
                        P::Ref(l, m, i) => P::Ref(l.clone(), *m, Box::new(holes(i))),
                        P::Tuple(v) => P::Tuple(v.iter().map(holes).collect()),
                        P::Path(q, p) => {
                            let mut p = p.clone();
                            for s in &mut p.segs {
                                for a in &mut s.args {
                                    if let GArg::Ty(t) = a {
                                        *t = holes(t);
                                    }
                                }
                            }
                            P::Path(q.clone(), p)
                        }
                        t => t.clone(),
                    }
                }
                let ts = crate::ast_types::tysyn(&holes(t), None).ok()?;
                prog.resolve_env(&ts, &env).ok()
            });
            // (`-> impl Iterator<Item = u32> { 0..3 }`: a range at the tail is a Range of the declared Item, so its
            // literals take that type)
            let decl = decl.or_else(|| {
                use crate::parse::{Bound, Expr, GArg, Stmt, Ty as P};
                let Some(P::Impl(bs)) = &f.sig.ret else { return None };
                let item = bs.iter().find_map(|b| match b {
                    Bound::Trait(_, p) if p.segs.last().is_some_and(|s| s.name == "Iterator") => p.segs.last()?.args.iter().find_map(|a| match a {
                        GArg::Bind(n, t) if n == "Item" => Some(t.clone()),
                        _ => None,
                    }),
                    _ => None,
                })?;
                let Some(Stmt::Expr(Expr::Range(Some(_), Some(_), incl), false)) = f.block.stmts.last() else { return None };
                let rn = if *incl { "RangeInclusive" } else { "Range" };
                let it = prog.resolve_env(&crate::ast_types::tysyn(&item, None).ok()?, &env).ok()?;
                let rid = *prog.struct_index.get(rn)?;
                prog.instantiate(rid, vec![it]).ok().map(Ty::Struct)
            });
            let (_, r, _) = crate::ast_infer::infer_ctx(prog, f.sig.recv.is_some(), &f.sig.params, &f.block, &sig, &env, &[], decl, Some((id, 0)));
            let sig = match decl {
                Some(d) => crate::program::FnSig { ret: d, ..sig },
                None => sig,
            };
            // (a body with no value (`-> impl Sized {}`) returns `()`)
            let no_value = !matches!(f.block.stmts.last(), Some(crate::parse::Stmt::Expr(_, false))) && !format!("{:?}", f.block).contains("Return(Some");
            // (a body ending in `()` itself, or in a diverging macro (`panic!()` / `todo!()`), is `()` too)
            let unit_tail = match f.block.stmts.last() {
                Some(crate::parse::Stmt::Expr(crate::parse::Expr::Tuple(v), false)) => v.is_empty(),
                // (`async {}` / `{ f(); }`: a block with no value of its own)
                Some(crate::parse::Stmt::Expr(crate::parse::Expr::Block(_, _, b), false)) => !matches!(b.stmts.last(), Some(crate::parse::Stmt::Expr(_, false))),
                Some(crate::parse::Stmt::Expr(crate::parse::Expr::Macro(p, _, _), _)) => {
                    matches!(p.segs.last().map(|s| s.name.as_str()), Some("panic" | "todo" | "unimplemented" | "unreachable"))
                }
                _ => false,
            };
            let no_value = no_value || unit_tail;
            // (`-> Vec<impl Debug> { panic!() }`: nothing says what the type is)
            if no_value && !matches!(f.sig.ret, Some(crate::parse::Ty::Impl(_))) {
                return Err(format!("{}:1: type annotations needed (the return type of `{}` is not known from its body)", items[ii as usize].line, f.sig.name));
            }
            let r = r.map(|t| merge_declared(prog, sig.ret, t));
            if let Some(t) = r.filter(|&t| (t != Ty::Unit || no_value) && !prog.has_param(t) && !has_hole_ty(t)) {
                prog.sigs[id as usize].ret = t;
                done[k] = true;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    // (`fn test() -> impl Sized { test() }`: a body that only returns its own result has no type of its own, and
    // never returns; it is `()`)
    for (k, &id) in ids.iter().enumerate() {
        if done[k] {
            continue;
        }
        let me = prog.names[id as usize].rsplit("::").next().unwrap_or("").to_string();
        let Some(ii) = prog.item_of[id as usize] else { continue };
        let Some(f) = parse_fn(src, &items[ii as usize])? else { continue };
        if f.block.stmts.len() == 1 && matches!(f.sig.ret, Some(crate::parse::Ty::Impl(_))) {
            if let Some(crate::parse::Stmt::Expr(crate::parse::Expr::Call(callee, _), false)) = f.block.stmts.last() {
                if matches!(&**callee, crate::parse::Expr::Path(None, p) if p.segs.len() == 1 && p.segs[0].name == me) {
                    prog.sigs[id as usize].ret = Ty::Unit;
                    done[k] = true;
                }
            }
        }
    }
    if let Some(k) = done.iter().position(|d| !*d) {
        return Err(format!("cannot infer the `impl Trait` return type of `{}`", prog.names[ids[k] as usize]));
    }
    Ok(())
}

/// アイテムの中の `行:桁:` のエラーを、ファイルの中の行にする (driver.rs の relocate_err と同じ)。位置がなければアイテムの先頭の行
fn relocate(e: String, it: &SrcItem) -> String {
    if let Some((l, rest)) = e.split_once(':') {
        if let Ok(l) = l.parse::<usize>() {
            return format!("{}:{rest}", l + it.line - 1);
        }
    }
    format!("{}:1: {e}", it.line)
}

/// アイテムの関数を自前の構文木で読む (本体がなければ None)。引数のパターンは本体の先頭の let にする
/// `#![deny(unused_imports)]`: the first name a top-level `use` brings in that no other item mentions. Traits
/// (std's usual ones and the program's) are left alone: their methods use them without naming them
fn first_unused_import(src: &str, items: &[SrcItem]) -> Option<(usize, usize, String)> {
    const TRAITS: &[&str] = &[
        "Read", "Write", "BufRead", "Seek", "Hash", "Hasher", "BuildHasher", "FromStr", "Display", "Debug", "Iterator", "IntoIterator",
        "DoubleEndedIterator", "ExactSizeIterator", "FromIterator", "Extend", "Deref", "DerefMut", "Borrow", "BorrowMut", "AsRef", "AsMut",
        "Error", "Any", "TryFrom", "TryInto", "From", "Into", "ToString", "ToOwned", "Ord", "PartialOrd", "Eq", "PartialEq", "Add", "Sub",
        "Mul", "Div", "Rem", "Neg", "Not", "AddAssign", "SubAssign", "MulAssign", "DivAssign", "Index", "IndexMut", "Drop", "Clone", "Copy",
        "Default", "Fn", "FnMut", "FnOnce", "Future", "IntoFuture", "Sum", "Product", "Step", "Termination", "Pointer", "LowerHex", "UpperHex",
        "Octal", "Binary", "LowerExp", "UpperExp", "Send", "Sync", "Sized", "Unpin", "SliceIndex", "Pattern", "ToSocketAddrs", "OsStrExt",
        "OsStringExt", "PermissionsExt", "MetadataExt", "FileExt", "CommandExt", "ExitStatusExt", "AsRawFd", "FromRawFd", "IntoRawFd",
    ];
    let (Some(first), Some(last)) = (items.first(), items.last()) else { return None };
    let region = &src[first.start..last.end];
    let b = region.as_bytes();
    let toks = crate::lexer::tokenize(b);
    let word = |k: usize| -> &str { if toks[k].0 == 0 { &region[toks[k].1..toks[k].2] } else { "" } };
    // (names mentioned outside `use` items, and the program's own traits)
    let mut used: crate::fxhash::FxSet<&str> = Default::default();
    let mut traits: crate::fxhash::FxSet<&str> = Default::default();
    let mut in_use = false;
    for k in 0..toks.len() {
        let w = word(k);
        if w == "use" {
            in_use = true;
        }
        if in_use {
            if toks[k].0 == 1 && b[toks[k].1] == b';' {
                in_use = false;
            }
            continue;
        }
        if k > 0 && word(k - 1) == "trait" {
            traits.insert(w);
        }
        if toks[k].0 == 0 {
            used.insert(w);
        }
    }
    for it in items {
        if !matches!(it.kind, crate::lexer::ItemKind::Normal) {
            continue;
        }
        let text = &src[it.start..it.end];
        let t = text.trim_start();
        if !t.starts_with("use ") || text.contains("allow(unused") || text.contains("#[cfg") || text.contains('*') {
            continue;
        }
        let body = t["use ".len()..].trim_end().trim_end_matches(';').trim();
        // (the names it brings in: `a::b::C`, `a::C as D`, `a::{B, C as D}` (one level of braces))
        let (prefix, list): (&str, Vec<&str>) = match body.find('{') {
            Some(i) if body.ends_with('}') => (&body[..i], body[i + 1..body.len() - 1].split(',').map(str::trim).filter(|x| !x.is_empty()).collect()),
            Some(_) => continue,
            None => ("", vec![body]),
        };
        let line = it.line + text[..text.len() - t.len()].matches('\n').count();
        for entry in &list {
            if entry.contains('{') || entry.contains('}') {
                break;
            }
            let (path, alias) = match entry.split_once(" as ") {
                Some((p, a)) => (p.trim(), Some(a.trim())),
                None => (entry.trim(), None),
            };
            let name = alias.unwrap_or_else(|| path.rsplit("::").next().unwrap_or(path));
            if name == "_" || name == "self" || name.is_empty() || TRAITS.contains(&name) || traits.contains(name) {
                continue;
            }
            if !used.contains(name) {
                let shown = if list.len() == 1 && prefix.is_empty() { (*entry).to_string() } else { (*entry).to_string() };
                return Some((line, 1, format!("unused import: `{shown}`")));
            }
        }
    }
    None
}

/// `#![deny(unreachable_code)]`: the first statement of a block after one that always diverges (`return`,
/// `break`, `continue`, `panic!` and its kin, a `loop` with no `break`), and whether it is a statement or the
/// block's value
fn first_unreachable(b: &crate::parse::Block) -> Option<(crate::parse::Pos, &'static str)> {
    use crate::parse::{Expr, Stmt};
    fn diverges(e: &Expr) -> bool {
        match e {
            Expr::Return(_) | Expr::Break(..) | Expr::Continue(_) => true,
            Expr::Macro(p, _, _) => matches!(p.segs.last().map(|s| s.name.as_str()), Some("panic" | "unreachable" | "todo" | "unimplemented")),
            Expr::Loop(_, body) => !format!("{body:?}").contains("Break("),
            Expr::Paren(x) => diverges(x),
            _ => false,
        }
    }
    fn block(b: &crate::parse::Block) -> Option<(crate::parse::Pos, &'static str)> {
        let n = b.stmts.len();
        for (i, s) in b.stmts.iter().enumerate() {
            let e = match s {
                Stmt::Expr(e, _) => Some(e),
                Stmt::Let(_, _, Some(e), _, _) => Some(e),
                _ => None,
            };
            if let Some(e) = e {
                if let Some(r) = inner(e) {
                    return Some(r);
                }
            }
            let div = match s {
                Stmt::Expr(e, _) => diverges(e),
                Stmt::Let(_, _, Some(e), None, _) => diverges(e),
                _ => false,
            };
            if div {
                // (the next statement that is not an item)
                for (j, t) in b.stmts.iter().enumerate().skip(i + 1) {
                    match t {
                        Stmt::Item(_) => continue,
                        Stmt::Let(_, _, _, _, p) => return Some((*p, "statement")),
                        Stmt::Expr(e, semi) => {
                            let what = if !*semi && j + 1 == n { "expression" } else { "statement" };
                            return Some((acg::expr_start(e), what));
                        }
                    }
                }
                return None;
            }
        }
        None
    }
    fn inner(e: &Expr) -> Option<(crate::parse::Pos, &'static str)> {
        match e {
            Expr::Block(_, _, b) | Expr::Loop(_, b) => block(b),
            Expr::If(_, t, els) => block(t).or_else(|| els.as_deref().and_then(inner)),
            Expr::While(_, _, b) | Expr::For(_, _, _, b) => block(b),
            _ => None,
        }
    }
    block(b)
}

/// "malformed `X` attribute input" (E0539 / E0565): a built-in attribute of a fixed shape written in another one.
/// Read are the file's inner attributes (its head) and the attributes in front of the items (no full-text scan)
fn malformed_attr(src0: &str, split: &str, items: &[SrcItem], standalone: bool) -> Option<String> {
    // (a crate with no dependency (so no proc macro) knows only the built-in attributes and its tools' paths)
    // (both looked up only when such an attribute is there: not a search of every file)
    let rustc_attrs = || src0.contains("rustc_attrs");
    let unknown = |name: &str| {
        const BUILTIN: &[&str] = &[
            "cfg", "cfg_attr", "test", "ignore", "should_panic", "bench", "derive", "automatically_derived", "macro_export", "macro_use",
            "proc_macro", "proc_macro_derive", "proc_macro_attribute", "allow", "warn", "deny", "forbid", "expect", "deprecated", "must_use",
            "diagnostic", "link", "link_name", "link_ordinal", "no_link", "repr", "export_name", "link_section", "no_mangle", "used", "crate_name",
            "crate_type", "no_builtins", "no_main", "no_std", "no_implicit_prelude", "recursion_limit", "type_length_limit", "windows_subsystem",
            "inline", "cold", "no_builtins", "target_feature", "track_caller", "instruction_set", "doc", "path", "non_exhaustive", "panic_handler",
            "global_allocator", "default_lib_allocator", "feature", "unsafe", "naked", "debugger_visualizer", "collapse_debuginfo", "coverage",
            "optimize", "rustfmt", "clippy", "unstable", "stable", "rustc_const_stable", "rustc_const_unstable", "register_tool", "macro_escape",
            "may_dangle", "thread_local", "ffi_pure", "ffi_const", "linkage", "omit_gdb_pretty_printer_section", "patchable_function_entry",
            "fundamental", "marker", "lang", "start", "custom_mir", "const_trait", "do_not_recommend", "on_unimplemented", "pointee",
            "type_const", "define_opaque", "align", "loop_match", "const_continue", "sanitize", "no_sanitize", "export_stable", "rustc_align",
            "async_trait", "allow_internal_unstable", "allow_internal_unsafe", "compiler_builtins", "needs_allocator", "panic_runtime",
            "needs_panic_runtime", "profiler_runtime", "no_core", "prelude_import", "rustc_std_internal_symbol", "unstable_feature_bound",
            "rustc_main", "test_runner", "reexport_test_harness_main", "default", "main", "plugin", "plugin_registrar",
        ];
        if name.contains("::") || name.is_empty() || (name.starts_with("rustc_") && rustc_attrs()) {
            return false;
        }
        // (`#[default]` is the derive(Default) helper, `#[main]` was removed)
        if name == "main" || name == "default" && !src0.contains("Default") {
            return true;
        }
        !BUILTIN.contains(&name)
    };
    // (an attribute's name and its input: None = a word, Some('(') = a list, Some('=') = `= value`)
    fn check(name: &str, input: &str) -> bool {
        let input = input.trim();
        let (form, inner) = if input.is_empty() {
            (' ', "")
        } else if let Some(r) = input.strip_prefix('(') {
            ('(', r.strip_suffix(')').unwrap_or(r).trim())
        } else if let Some(r) = input.strip_prefix('=') {
            ('=', r.trim())
        } else {
            return true;
        };
        let str_lit = |v: &str| v.starts_with('"') && v.ends_with('"') && v.len() >= 2;
        let idents = |v: &str| v.split(',').all(|x| {
            let x = x.trim();
            !x.is_empty() && x.chars().all(|c| c.is_alphanumeric() || c == '_')
        });
        match name {
            "crate_type" | "recursion_limit" | "type_length_limit" | "crate_name" => form == '=' && str_lit(inner),
            "inline" => form == ' ' || (form == '(' && matches!(inner, "always" | "never")),
            "must_use" => form == ' ' || (form == '=' && str_lit(inner)),
            "cold" | "track_caller" | "non_exhaustive" | "no_mangle" | "automatically_derived" | "no_implicit_prelude" => form == ' ',
            "repr" => form == '(' && !inner.is_empty() && !inner.contains('='),
            "deprecated" => form == ' ' || (form == '=' && str_lit(inner)) || form == '(',
            "link" => form == '(' && !inner.is_empty() && !inner.contains("cfg()"),
            "allow" | "warn" | "deny" | "forbid" | "expect" => form == '(',
            "macro_use" => form == ' ' || (form == '(' && idents(inner)),
            "cfg" => form == '(' && !inner.is_empty(),
            "cfg_attr" => form == '(' && inner.contains(','),
            _ => true,
        }
    }
    // (the attributes at the start of a text: `#[name input]` / `#![name input]`, with their offsets)
    fn leading(t: &str) -> Vec<(usize, &str, &str)> {
        let mut out = vec![];
        let mut i = 0;
        loop {
            let rest = &t[i..];
            let ws = rest.len() - rest.trim_start().len();
            let r = &rest[ws..];
            let open = if r.starts_with("#![") { 3 } else if r.starts_with("#[") { 2 } else { break };
            // (the matching `]`, past nested brackets and string literals)
            let mut depth = 0i32;
            let mut in_str = false;
            let mut end = None;
            for (k, c) in r.char_indices().skip(open) {
                match c {
                    '"' => in_str = !in_str,
                    '[' | '(' if !in_str => depth += 1,
                    ')' if !in_str => depth -= 1,
                    ']' if !in_str && depth == 0 => {
                        end = Some(k);
                        break;
                    }
                    ']' if !in_str => depth -= 1,
                    _ => {}
                }
            }
            let Some(e) = end else { break };
            let body = &r[open..e];
            let n_end = body.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')).unwrap_or(body.len());
            out.push((i + ws, &body[..n_end], &body[n_end..]));
            i += ws + e + 1;
        }
        out
    }
    let report = |text: &str, base_line: usize, at: usize, name: &str| {
        let line = base_line + text[..at].matches('\n').count();
        format!("{line}:1: malformed `{name}` attribute input")
    };
    // (the file's inner attributes)
    let mut head_end = src0.len().min(8192);
    while !src0.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let head = &src0[..head_end];
    // (comments between them: line by line, skipping `//` lines)
    let mut off = 0;
    for line in head.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("#![") {
            for (at, name, input) in leading(line) {
                if !check(name, input) {
                    return Some(report(head, 1, off + at, name));
                }
            }
        } else if !(t.is_empty() || t.starts_with("//")) && !t.starts_with("#[") {
            break;
        }
        off += line.len();
    }
    for it in items {
        let text = &split[it.start..it.end];
        if !text.trim_start().starts_with("#[") {
            continue;
        }
        let attrs = leading(text);
        for &(at, name, input) in &attrs {
            if !check(name, input) {
                return Some(report(text, it.line, at, name));
            }
            // (a parse error, as a merge's diff marker, comes first in rustc)
            if standalone && unknown(name) && !text.contains("<<<<<<<") {
                let line = it.line + text[..at].matches('\n').count();
                return Some(format!("{line}:1: cannot find attribute `{name}` in this scope"));
            }
        }
        // (an attribute on an item of a kind it does not apply to: `#[inline] struct S;`)
        let after = attrs.last().map(|&(at, _, _)| {
            let r = &text[at..];
            let e = r.find(']').map(|k| at + k + 1).unwrap_or(text.len());
            // (past comment lines between the attributes and the item)
            let mut r = text[e..].trim_start();
            while r.starts_with("//") {
                r = r[r.find('\n').unwrap_or(r.len())..].trim_start();
            }
            r
        });
        let Some(after) = after else { continue };
        let mut ws = after.split_whitespace().filter(|w| !matches!(*w, "pub" | "unsafe" | "async" | "const" | "extern" | "default") && !w.starts_with("pub(") && !w.starts_with('"'));
        let Some(kw) = ws.next() else { continue };
        let kw = if after.starts_with("const ") && !after.starts_with("const fn") { "const" } else { kw };
        let kind = match kw {
            "fn" => "functions",
            "struct" => "structs",
            "enum" => "enums",
            "union" => "unions",
            "trait" => "traits",
            "impl" => "inherent impl blocks",
            "mod" => "modules",
            "static" => "statics",
            "const" => "constants",
            "type" => "type aliases",
            "use" => "use statements",
            "macro_rules!" => "macro defs",
            _ => continue,
        };
        for &(at, name, _) in &attrs {
            let ok = match name {
                "inline" | "cold" | "track_caller" => kw == "fn",
                "repr" => matches!(kw, "struct" | "enum" | "union"),
                "non_exhaustive" => matches!(kw, "struct" | "enum"),
                "must_use" => matches!(kw, "fn" | "struct" | "enum" | "union" | "trait"),
                "macro_use" => matches!(kw, "mod"),
                "no_mangle" => matches!(kw, "fn" | "static"),
                _ => true,
            };
            if !ok {
                let line = it.line + text[..at].matches('\n').count();
                return Some(format!("{line}:1: `#[{name}]` attribute cannot be used on {kind}"));
            }
        }
    }
    None
}

/// E0252 / E0255: a name a top-level `use` brings in that another top-level `use` or item of the crate root also
/// defines ("the name `X` is defined multiple times", at the second). Top level = the item starts a line (the
/// flattened modules' items keep their indentation)
fn duplicate_import(src0: &str, split: &str, items: &[SrcItem]) -> Option<String> {
    // (no crate-root `use`, no clash to look for: the common case costs one look at each item's first bytes)
    if !items.iter().any(|it| {
        matches!(it.kind, ItemKind::Normal)
            && (it.start == 0 || split.as_bytes()[it.start - 1] == b'\n')
            && {
                // (past the item's attributes, which only a few items have)
                let mut t = &split[it.start..it.end];
                while t.starts_with("#[") {
                    match t.find(']') {
                        Some(e) => t = t[e + 1..].trim_start(),
                        None => break,
                    }
                }
                t.starts_with("use ") || t.starts_with("pub use ")
            }
    }) {
        return None;
    }
    // (name → (some definition is a `use`, some definition is an item, a line): a hash, as a crate has thousands of items)
    let mut seen: FxMap<&str, (bool, bool, usize)> = FxMap::default();
    // (the original file's last line: items past it were hoisted out of fn bodies, and are their blocks')
    let mut total: Option<usize> = None;
    // (reused for each item: no allocation per item)
    let mut names: Vec<(&str, bool)> = vec![];
    for it in items {
        if !matches!(it.kind, ItemKind::Normal) || !(it.start == 0 || split.as_bytes()[it.start - 1] == b'\n') {
            continue;
        }
        let text = &split[it.start..it.end];
        if text.starts_with([' ', '\t']) || text.starts_with("#!") || (text.starts_with('#') && text.contains("#[cfg")) {
            continue;
        }
        // (past the item's attributes: `#[allow(unused_imports)]`)
        let mut t = text;
        while t.starts_with("#[") {
            let Some(e) = t.find(']') else { break };
            t = t[e + 1..].trim_start();
        }
        let line_shift = text[..text.len() - t.len()].matches('\n').count();
        let t = t.strip_prefix("pub ").unwrap_or(t);
        names.clear();
        if let Some(rest) = t.strip_prefix("use ") {
            let body = rest.trim_end().trim_end_matches(';').trim();
            let (prefix, list): (&str, Vec<&str>) = match body.find('{') {
                Some(i) if body.ends_with('}') && !body[i + 1..].contains('{') => (&body[..i], body[i + 1..body.len() - 1].split(',').map(str::trim).filter(|x| !x.is_empty()).collect()),
                Some(_) => continue,
                None => ("", vec![body]),
            };
            for e in list {
                let (path, alias) = match e.split_once(" as ") {
                    Some((p, a)) => (p.trim(), Some(a.trim())),
                    None => (e.trim(), None),
                };
                // (`use a::{self}` names `a`)
                let last = if path == "self" { prefix.trim_end_matches("::").rsplit("::").next().unwrap_or("") } else { path.rsplit("::").next().unwrap_or(path) };
                let n = alias.unwrap_or(last);
                if !n.is_empty() && n != "_" && n != "*" && !path.ends_with('*') {
                    names.push((n, true));
                }
            }
        } else {
            let mut ws = t.split_whitespace();
            if let (Some(kw), Some(name)) = (ws.next(), ws.next()) {
                if matches!(kw, "struct" | "enum" | "trait" | "type" | "union" | "fn" | "const" | "static") {
                    let end = name.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(name.len());
                    // (a user type renamed off a prelude type's name is still that name)
                    let n = &name[..end];
                    let n = n.strip_prefix("__u_").unwrap_or(n);
                    if !n.is_empty() && !n.contains("__") {
                        names.push((n, false));
                    }
                }
            }
        }
        let line = it.line + line_shift;
        for &(n, is_use) in &names {
            // (two items of one name are the build's own duplicate check; here at least one is a `use`)
            if let Some(&(any_use, any_item, l0)) = seen.get(&n) {
                if (any_use || is_use) && (any_use || any_item) {
                    let last = *total.get_or_insert_with(|| src0.bytes().filter(|&c| c == b'\n').count() + 1);
                    if line <= last && l0 <= last {
                        return Some(format!("{line}:1: the name `{n}` is defined multiple times"));
                    }
                }
            }
            let e = seen.entry(n).or_insert((false, false, line));
            if is_use {
                e.0 = true;
            } else {
                e.1 = true;
            }
        }
    }
    None
}

/// A top-level `use a::..` (at the start of a line) whose first segment is no crate the build knows (std, core,
/// a dependency, an `extern crate`), no module or item of the program, nor `crate` / `self` / `super`:
/// "unresolved import `a`" (E0432). Only read when such a `use` is there at all
fn unresolved_import(src: &str, split: &str, items: &[SrcItem], deps: &[String]) -> Option<String> {
    // (and the names std's prelude brings: `use Default::default` / `use Option::*`)
    let known = |n: &str| {
        matches!(
            n,
            "std" | "core" | "crate" | "self" | "super" | "" | "proc_macro" | "Default" | "Option" | "Result" | "Some" | "None" | "Ok" | "Err"
                | "Vec" | "String" | "Box" | "Iterator" | "IntoIterator" | "ToString" | "From" | "Into" | "Clone" | "Copy" | "Drop" | "Fn" | "FnMut"
                | "FnOnce" | "PartialEq" | "Eq" | "PartialOrd" | "Ord" | "AsRef" | "AsMut" | "Send" | "Sync" | "Sized" | "ToOwned" | "Extend"
        ) || deps.iter().any(|d| d == n)
    };
    let mut firsts: Vec<(usize, String)> = vec![];
    let mut seconds: Vec<(usize, String, String)> = vec![];
    for it in items {
        if !matches!(it.kind, ItemKind::Normal) {
            continue;
        }
        let text = &split[it.start..it.end];
        let t = text.trim_start();
        let t = t.strip_prefix("pub ").unwrap_or(t);
        let Some(rest) = t.strip_prefix("use ") else { continue };
        let rest = rest.trim_start().trim_start_matches("::");
        let first: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        // (a fn's own `use`, hoisted out with a `__h<N>` suffix, is resolved in its block)
        if !known(&first) && !first.starts_with("r#") && !first.contains("__") {
            let line = it.line + text[..text.len() - text.trim_start().len()].matches('\n').count();
            // (`use m::f..`: the second segment, for a module of the program's)
            if let Some(r2) = rest[first.len()..].strip_prefix("::") {
                let second: String = r2.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                if !second.is_empty() && second != "self" && second != "super" {
                    seconds.push((line, first.clone(), second));
                }
            }
            firsts.push((line, first));
        }
    }
    if firsts.is_empty() {
        return None;
    }
    // (whether the program defines or brings in a name: a few substring searches per distinct candidate, not a
    // tokenization of the whole file)
    let b = src.as_bytes();
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let defines = |n: &str| {
        ["mod", "struct", "enum", "trait", "fn", "type", "const", "static", "union", "crate", "as", "macro_rules!"].iter().any(|kw| {
            let pat = format!("{kw} {n}");
            src.match_indices(pat.as_str()).any(|(i, _)| {
                let before_ok = i == 0 || !is_id(b[i - 1]);
                let after_ok = b.get(i + pat.len()).is_none_or(|&c| !is_id(c));
                // (not in a `//` comment: `//~| SUGGESTION extern crate foo;`)
                let ls = src[..i].rfind('\n').map(|k| k + 1).unwrap_or(0);
                before_ok && after_ok && !src[ls..i].contains("//")
            })
        })
    };
    let mut defined: crate::fxhash::FxSet<String> = Default::default();
    for (_, f) in &firsts {
        if defines(f) {
            defined.insert(f.clone());
        }
    }
    // (a macro could write items: leave such programs alone)
    if src.contains("macro_rules!") && firsts.iter().any(|(_, f)| !defined.contains(f)) && src.contains("$") {
        return None;
    }
    if let Some((line, f)) = firsts.into_iter().find(|(_, f)| !defined.contains(f)) {
        return Some(format!("{line}:1: unresolved import `{f}` (E0432)"));
    }
    // (`use m::f` with an inline `mod m { .. }` that defines no `f`)
    for (line, m, f) in seconds {
        let pat = format!("mod {m} {{");
        let Some(at) = src.match_indices(pat.as_str()).map(|(i, _)| i).find(|&i| i == 0 || !is_id(b[i - 1])) else { continue };
        let open = at + pat.len();
        let mut depth = 1;
        let mut end = None;
        for (k, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + k);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { continue };
        let body = &src[open..end];
        // (defined there, or brought in by a `use` of it, or written by a macro / glob)
        let bb = body.as_bytes();
        let named = ["mod", "struct", "enum", "trait", "fn", "type", "const", "static", "union", "as", "macro_rules!"].iter().any(|kw| {
            let p = format!("{kw} {f}");
            body.match_indices(p.as_str()).any(|(i, _)| (i == 0 || !is_id(bb[i - 1])) && bb.get(i + p.len()).is_none_or(|&c| !is_id(c)))
        }) || [format!("::{f};"), format!("::{f},"), format!("::{f} "), format!("{{{f}"), format!(" {f},"), format!(" {f}}}")].iter().any(|p| body.contains(p.as_str()))
            || body.contains('*')
            || body.contains('$')
            || body.contains("include!");
        if !named {
            return Some(format!("{line}:1: unresolved import `{m}::{f}` (E0432)"));
        }
    }
    None
}

/// The naming lints (`deny` = [non_snake_case, non_camel_case_types, non_upper_case_globals]): the first fn / method,
/// type / trait or static / const of the program whose name breaks its case convention, with rustc's message
fn first_bad_name(src: &str, items: &[SrcItem], deny: [bool; 3]) -> Option<(usize, String)> {
    let snake = |n: &str| !n.chars().any(|c| c.is_uppercase());
    let camel = |n: &str| {
        let t = n.trim_matches('_');
        t.is_empty()
            || (!t.chars().next().is_some_and(|c| c.is_lowercase())
                && !t.contains("__")
                && !t.as_bytes().windows(2).any(|w| w[0] == b'_' && !w[1].is_ascii_digit()))
    };
    let upper = |n: &str| !n.chars().any(|c| c.is_lowercase());
    for it in items {
        let text = &src[it.start..it.end];
        if text.contains("allow(non_") || text.contains("expect(non_") || text.contains("allow(nonstandard_style") || text.contains("#[cfg") || text.contains("no_mangle")
            || text.contains("extern \"C\"") || text.contains("export_name")
        {
            continue;
        }
        let t = crate::lexer::tokenize(text.as_bytes());
        let tw = |k: usize| -> &str { if t[k].0 == 0 { &text[t[k].1..t[k].2] } else { "" } };
        let mut k = 0;
        while k < t.len() && (t[k].0 != 0 || matches!(tw(k), "pub" | "unsafe" | "async" | "crate" | "default")) {
            // (skip attributes and visibility: `#[..]`, `pub(crate)`)
            if tw(k) == "impl" {
                break;
            }
            k += 1;
        }
        if k + 1 >= t.len() {
            continue;
        }
        let kw = tw(k);
        let kw = if kw == "const" && tw(k + 1) == "fn" { "fn" } else { kw };
        let nk = if tw(k) == "const" && tw(k + 1) == "fn" { k + 2 } else { k + 1 };
        if nk >= t.len() {
            continue;
        }
        let raw = tw(nk);
        // (a fn-local item hoisted out has a `__h<N>` suffix; a module's item a `mod__` prefix)
        let raw = match raw.rfind("__h") {
            Some(i) if i > 0 && raw[i + 3..].chars().all(|c| c.is_ascii_digit()) && raw.len() > i + 3 => &raw[..i],
            _ => raw,
        };
        let name = raw.rsplit("__").next().unwrap_or(raw);
        let name = if raw.contains("__") { name.trim_start_matches("u_") } else { raw };
        if name.is_empty() || name == "_" || name.starts_with("r#") {
            continue;
        }
        let member = matches!(it.kind, ItemKind::ImplMember(_) | ItemKind::TraitMember(_));
        // (a struct's fields: `X: usize` is "structure field `X`")
        if (kw == "struct" || kw == "enum") && deny[0] {
            // (an enum's struct-like variants: their fields one level deeper)
            let field_depth = if kw == "enum" { 2 } else { 1 };
            let mut depth = 0;
            for j in nk + 1..t.len() {
                if t[j].0 == 1 {
                    match text.as_bytes()[t[j].1] {
                        b'{' | b'(' | b'<' | b'[' => depth += 1,
                        b'}' | b')' | b'>' | b']' => depth -= 1,
                        _ => {}
                    }
                    continue;
                }
                let f = tw(j);
                if depth == field_depth && j + 1 < t.len() && t[j + 1].0 == 1 && text.as_bytes()[t[j + 1].1] == b':' && f != "pub" && !snake(f) && !f.starts_with('_') {
                    let line = it.line + text[..t[j].1].matches('\n').count();
                    return Some((line, format!("structure field `{f}` should have a snake case name")));
                }
            }
        }
        // (lifetime parameters in the header: `fn f<'FooBar>` / `struct S<'Ab>`)
        if deny[0] {
            let head = &text[..text.find('{').unwrap_or(text.len())];
            let hb = head.as_bytes();
            let mut i = 0;
            while let Some(k) = head[i..].find('\'') {
                let a = i + k + 1;
                let e = a + head[a..].find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(head.len() - a);
                // (a lifetime, not a char literal `'a'`)
                if e > a && hb.get(e) != Some(&b'\'') {
                    let l = &head[a..e];
                    if l != "static" && !snake(l) {
                        let line = it.line + head[..a].matches('\n').count();
                        return Some((line, format!("lifetime `'{l}` should have a snake case name")));
                    }
                }
                i = e.max(a);
            }
        }
        // (a fn's parameters and `let`s: "variable `X`")
        if kw == "fn" && deny[0] {
            if let Ok(Some(f)) = parse_fn(src, it) {
                for (p, _) in &f.sig.params {
                    if let Pat::Ident(_, _, n, None) = p {
                        if !snake(n) && !n.starts_with("__exr") {
                            return Some((it.line, format!("variable `{n}` should have a snake case name")));
                        }
                    }
                }
                for (pos, n) in crate::ast_closure::let_bindings(&f.block) {
                    // (an upper-case name in a pattern may be a constant or a unit struct: only clear variables)
                    if !snake(&n) && n.chars().any(|c| c.is_lowercase()) && !n.starts_with("__") && pos.line > 0 {
                        return Some((it.line + pos.line as usize - 1, format!("variable `{n}` should have a snake case name")));
                    }
                }
            }
        }
        let in_trait = matches!(it.kind, ItemKind::TraitMember(_));
        let msg = match kw {
            "fn" if deny[0] && !snake(name) => {
                let what = if in_trait { "trait method" } else if member { "method" } else { "function" };
                format!("{what} `{name}` should have a snake case name")
            }
            "struct" | "enum" | "union" | "type" if deny[1] && !camel(name) => {
                let what = if kw == "type" && member { "associated type" } else { "type" };
                format!("{what} `{name}` should have an upper camel case name")
            }
            "trait" if deny[1] && !camel(name) => format!("trait `{name}` should have an upper camel case name"),
            "static" if deny[2] && !upper(name) => format!("static variable `{name}` should have an upper case name"),
            "const" if deny[2] && !upper(name) => {
                let what = if member { "associated constant" } else { "constant" };
                format!("{what} `{name}` should have an upper case name")
            }
            _ => continue,
        };
        let line = it.line + text[..t[nk].1].matches('\n').count();
        return Some((line, msg));
    }
    None
}

/// `#![deny(dead_code)]`: the first top-level item of the program that nothing uses (see the caller)
fn first_dead_item(src: &str, items: &[SrcItem], check_pub: bool) -> Option<(usize, String)> {
    let (Some(first), Some(last)) = (items.first(), items.last()) else { return None };
    let region = &src[first.start..last.end];
    let b = region.as_bytes();
    let toks = crate::lexer::tokenize(b);
    let word = |k: usize| -> &str { if toks[k].0 == 0 { &region[toks[k].1..toks[k].2] } else { "" } };
    let sym = |k: usize, c: u8| toks[k].0 == 1 && b[toks[k].1] == c;
    // (uses of each name: every occurrence but the definition's, the one after `impl` / `for` in an impl header)
    let mut uses: crate::fxhash::FxMap<&str, usize> = Default::default();
    let mut in_impl_header = false;
    for k in 0..toks.len() {
        let w = word(k);
        if w == "impl" {
            in_impl_header = true;
        }
        if in_impl_header && sym(k, b'{') {
            in_impl_header = false;
        }
        if toks[k].0 != 0 {
            continue;
        }
        let defn = k > 0 && matches!(word(k - 1), "fn" | "struct" | "enum" | "const" | "static" | "trait" | "union" | "type");
        let impl_self = in_impl_header && k > 0 && (word(k - 1) == "impl" || word(k - 1) == "for" || sym(k - 1, b'>'));
        if !defn && !impl_self {
            *uses.entry(w).or_default() += 1;
        }
    }
    for it in items {
        // (top-level items only: impl and trait members are a type's, not free items)
        if !matches!(it.kind, crate::lexer::ItemKind::Normal) {
            continue;
        }
        let text = &src[it.start..it.end];
        if text.contains("allow(dead_code") || text.contains("expect(dead_code") || text.contains("allow(unused") || text.contains("no_mangle") || text.contains("#[test]")
            || text.contains("#[derive") || text.contains("export_name") || text.contains("#[cfg")
        {
            continue;
        }
        let t = crate::lexer::tokenize(text.as_bytes());
        let tw = |k: usize| -> &str { if t[k].0 == 0 { &text[t[k].1..t[k].2] } else { "" } };
        // (the item's keyword and name, after its attributes and visibility)
        let mut k = 0;
        while k < t.len() && (tw(k) == "pub" || tw(k) == "unsafe" || tw(k) == "async" || tw(k) == "extern" || t[k].0 != 0) {
            if tw(k) == "extern" || tw(k) == "impl" || tw(k) == "mod" || tw(k) == "use" || tw(k) == "macro_rules" {
                break;
            }
            k += 1;
        }
        if k + 1 >= t.len() {
            continue;
        }
        let (kw, name) = (tw(k), tw(k + 1));
        if !check_pub && (0..k).any(|j| tw(j) == "pub") {
            continue;
        }
        if name.is_empty() || name.starts_with('_') || name == "main" {
            continue;
        }
        // (the name as written: without the flattened modules' `a__` prefix and the renamer's `__u_`)
        let shown = name.rsplit("__").next().unwrap_or(name).trim_start_matches("u_");
        let shown = if name.contains("__") { shown } else { name };
        let what = match kw {
            "fn" => format!("function `{shown}` is never used"),
            "struct" => format!("struct `{shown}` is never constructed"),
            "enum" => format!("enum `{shown}` is never used"),
            "const" => format!("constant `{shown}` is never used"),
            "static" => format!("static `{shown}` is never used"),
            "trait" => format!("trait `{shown}` is never used"),
            _ => continue,
        };
        if uses.get(name).copied().unwrap_or(0) == 0 {
            let line = it.line + text[..t[k].1].matches('\n').count();
            return Some((line, what));
        }
    }
    None
}

pub fn parse_fn(src: &str, it: &SrcItem) -> R<Option<ItemFn>> {
    let text = &src[it.start..it.end];
    let f = crate::parse::parse_file(text).map_err(|e| relocate(e, it))?;
    let Some(item) = f.items.into_iter().next() else { return Ok(None) };
    let crate::parse::ItemKind::Fn(mut sig, Some(mut block)) = item.kind else { return Ok(None) };
    let mut lets = vec![];
    for (k, (p, _)) in sig.params.iter_mut().enumerate() {
        if matches!(p, Pat::Ident(..) | Pat::Wild) {
            continue;
        }
        let name = format!("__exr_arg{k}");
        let pat = std::mem::replace(p, Pat::Ident(false, false, name.clone(), None));
        lets.push(Stmt::Let(pat, None, Some(acg::path_expr(&name)), None, Default::default()));
    }
    if !lets.is_empty() {
        lets.append(&mut block.stmts);
        block.stmts = lets;
    }
    Ok(Some(ItemFn { attrs: item.attrs, sig, block }))
}

/// クロージャ式を関数にする (`fn call(__env: (), p0: (), ..) { 本体 }`: 型はシグネチャから決まる)。
/// in_unsafe: the closure is written where `unsafe` operations are allowed, so its body is put in `unsafe { }`
pub fn synth_closure(c: &Expr, in_unsafe: bool) -> ItemFn {
    let Expr::Closure(_, ps, _, body, _) = c else { unreachable!() };
    let unit = crate::parse::Ty::Tuple(vec![]);
    let mut params = vec![(Pat::Ident(false, false, "__env".into(), None), unit.clone())];
    let mut stmts = vec![];
    for (i, (p, _)) in ps.iter().enumerate() {
        let simple = matches!(p, Pat::Ident(false, _, _, None));
        let pin = if simple {
            p.clone()
        } else {
            let id = format!("__p{i}");
            stmts.push(Stmt::Let(p.clone(), None, Some(acg::path_expr(&id)), None, Default::default()));
            Pat::Ident(false, false, id, None)
        };
        params.push((pin, unit.clone()));
    }
    match &**body {
        Expr::Block(None, false, b) => stmts.extend(b.stmts.iter().cloned()),
        e => stmts.push(Stmt::Expr(e.clone(), false)),
    }
    if in_unsafe {
        stmts = vec![Stmt::Expr(Expr::Block(None, true, Block { stmts }), false)];
    }
    let sig = crate::parse::FnSig {
        name: "call".into(),
        generics: crate::parse::Generics { params: vec![], preds: vec![] },
        recv: None,
        recv_ty: None,
        params,
        ret: None,
        quals: vec![],
    };
    ItemFn { attrs: vec![], sig, block: Block { stmts } }
}

/// The text of an item after its attributes and comments, and the number of lines skipped to get there
fn after_attrs(text: &str) -> (&str, usize) {
    let mut t = text;
    loop {
        let u = t.trim_start();
        if let Some(r) = u.strip_prefix("//") {
            t = r.find('\n').map(|k| &r[k..]).unwrap_or("");
        } else if let Some(r) = u.strip_prefix("/*") {
            t = r.find("*/").map(|k| &r[k + 2..]).unwrap_or("");
        } else if let Some(r) = u.strip_prefix("#[").or_else(|| u.strip_prefix("#![")) {
            t = &r[attr_end(r)..];
        } else {
            t = u;
            break;
        }
    }
    let skipped = &text[..text.len() - t.len()];
    (t, skipped.matches('\n').count())
}

/// The length of an attribute's body up to and with its closing `]` (r is after `#[`). Brackets inside string, raw
/// string and char literals do not count: `#[doc = "]"]` ends at the last `]`
fn attr_end(r: &str) -> usize {
    let b = r.as_bytes();
    let mut depth = 1;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            // a raw string `r"..."` / `r#"..."#`: to the quote followed by as many `#`
            b'r' if i + 1 < b.len() && (b[i + 1] == b'"' || b[i + 1] == b'#') && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) => {
                let mut j = i + 1;
                let mut hashes = 0;
                while j < b.len() && b[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                if j < b.len() && b[j] == b'"' {
                    j += 1;
                    while j < b.len() && !(b[j] == b'"' && b[j + 1..].iter().take(hashes).filter(|&&c| c == b'#').count() == hashes) {
                        j += 1;
                    }
                    i = j + hashes;
                }
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            // a char literal ('x', '\'', '\\'): not a lifetime, which has no closing quote two or three bytes on
            b'\'' if b.get(i + 2) == Some(&b'\'') || (b.get(i + 1) == Some(&b'\\') && b.get(i + 3) == Some(&b'\'')) => {
                i += if b[i + 1] == b'\\' { 3 } else { 2 };
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// t without its `// ..` and `/* .. */` comments (each one a space)
fn strip_comments(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    loop {
        let (a, b) = (rest.find("//"), rest.find("/*"));
        let k = match (a, b) {
            (Some(x), Some(y)) => x.min(y),
            (Some(x), None) | (None, Some(x)) => x,
            (None, None) => break,
        };
        out.push_str(&rest[..k]);
        out.push(' ');
        rest = if rest[k..].starts_with("//") {
            rest[k..].find('\n').map(|e| &rest[k + e..]).unwrap_or("")
        } else {
            rest[k + 2..].find("*/").map(|e| &rest[k + 2 + e + 2..]).unwrap_or("")
        };
    }
    out.push_str(rest);
    out
}

/// `t` without the keyword `w` at its start (and the spaces after it)
fn after_word<'a>(t: &'a str, w: &str) -> Option<&'a str> {
    let r = t.strip_prefix(w)?;
    if r.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(r.trim_start())
}

/// E0200: an impl of an unsafe trait (`Send`, `Sync`, or an `unsafe trait` of the program) must be an `unsafe impl`.
/// Implementing `Send` for a type holding an `Rc` is how safe code would otherwise share it between threads
fn unsafe_impls(src: &str, items: &[SrcItem]) -> R<()> {
    let mut unsafe_traits: Vec<&str> = vec!["Send", "Sync"];
    for it in items {
        let (t, _) = after_attrs(src.get(it.start..it.end).unwrap_or(""));
        let t = after_word(t, "pub").map(|r| if r.starts_with('(') { r.find(')').map(|k| r[k + 1..].trim_start()).unwrap_or(r) } else { r }).unwrap_or(t);
        if let Some(r) = after_word(t, "unsafe").and_then(|r| after_word(r, "trait")) {
            let n = r.split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("");
            if !n.is_empty() {
                unsafe_traits.push(n);
            }
        }
    }
    for it in items {
        if !matches!(it.kind, ItemKind::Normal | ItemKind::ImplHeader) {
            continue;
        }
        let (t, skipped) = after_attrs(src.get(it.start..it.end).unwrap_or(""));
        let Some(mut r) = after_word(t, "impl") else { continue };
        // (the impl's own generics: `impl<T: Send> Send for W<T>`)
        if r.starts_with('<') {
            let mut depth = 0;
            let mut end = r.len();
            for (k, c) in r.char_indices() {
                match c {
                    '<' => depth += 1,
                    '>' if k > 0 && r.as_bytes()[k - 1] == b'-' => {}
                    '>' => {
                        depth -= 1;
                        if depth == 0 {
                            end = k + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            r = r[end..].trim_start();
        }
        // (the header without comments, in single spaces: `impl Send // ..\nfor S` is the same header)
        let head = strip_comments(r.split('{').next().unwrap_or("")).split_whitespace().collect::<Vec<_>>().join(" ");
        let Some(k) = head.find(" for ") else { continue };
        let path = head[..k].trim();
        let last = path.split('<').next().unwrap_or("").rsplit("::").next().unwrap_or("").trim();
        if unsafe_traits.contains(&last) {
            return Err(format!("{}:1: the trait `{last}` requires an `unsafe impl` declaration (E0200)", it.line + skipped));
        }
    }
    Ok(())
}

/// 関数 (インスタンスも) id の本体
fn fn_ast(a: &Analysis, body: u32) -> R<ItemFn> {
    let ii = a.prog.item_of[body as usize].ok_or("internal: no item")?;
    let it = &a.items[ii as usize];
    if let Some(cd) = a.prog.closures.get(&body) {
        let mut encl = a.encl.lock().unwrap();
        if !encl.contains_key(&ii) {
            let f = parse_fn(&a.src, it)?.ok_or_else(|| format!("`{}` has no body", a.prog.names[body as usize]))?;
            let u = crate::ast_closure::closures_in_unsafe(&f.block);
            encl.insert(ii, crate::ast_closure::closures_of(&f.block).into_iter().zip(u).collect());
        }
        let cs = encl.get(&ii).unwrap();
        let (c, in_unsafe) = cs.get(cd.k as usize).ok_or("internal: closure not found")?;
        // (a closure written in `unsafe { }` or in an `unsafe fn` may do what its surroundings may)
        let in_unsafe = *in_unsafe || a.prog.sigs.get(cd.parent as usize).is_some_and(|s| s.is_unsafe);
        return Ok(synth_closure(c, in_unsafe));
    }
    let f = parse_fn(&a.src, it)?.ok_or_else(|| format!("`{}` has no body", a.prog.names[body as usize]))?;
    Ok(f)
}

/// 展開してよい関数: prelude の (クロージャでも特別な本体でもない) 小さい関数
fn inline_bodies(a: &Analysis) -> acg::Inline {
    let p = &a.prog;
    let mut inl = acg::Inline::default();
    let special = [
        crate::prelude::DROP_GLUE, "__exr_hash", "__exr_cmp", "__exr_eq", "__exr_clone", "__exr_default", "__exr_display", "__exr_debug",
        "__exr_rt_init", "__exr_on_trap", "Option::unwrap", "Option::expect", "Result::unwrap", "Result::expect", "Result::unwrap_err", "Result::expect_err",
    ];
    for id in 0..p.names.len() {
        let Some(ii) = p.item_of[id] else { continue };
        // (依存するクレートの関数は展開しない: 本体はインターフェースのハッシュに入らないので、変わっても気付けない)
        if a.ext_items.get(ii as usize).copied().unwrap_or(false) || !p.sigs[id].has_body || p.sigs[id].is_extern {
            continue;
        }
        // (クロージャの本体: 大きさは本体の構文木で見る (アイテムは囲む関数のもの))
        if p.closures.contains_key(&(id as u32)) {
            if std::env::var_os("ZUSTC_NO_INLINE_CLOSURE").is_some() {
                continue;
            }
            if let Ok(f) = fn_ast(a, id as u32) {
                let ok = acg::inline_candidate(&f) && format!("{:?}", f.block).len() <= 2400;
                if std::env::var_os("ZUSTC_INLINE_LOG").is_some() {
                    eprintln!("inline {} closure {}", if ok { "yes" } else { "no " }, p.names[id]);
                }
                if ok {
                    inl.bodies.insert(id as u32, f);
                    let it = &a.items[ii as usize];
                    if ii < p.prelude_from {
                        inl.user_lines.insert(id as u32, it.line as u32 - 1);
                    }
                }
            }
            continue;
        }
        if special.contains(&p.names[id].as_str()) {
            continue;
        }
        let it = &a.items[ii as usize];
        if it.end - it.start > 4000 {
            continue;
        }
        // (大きさはソースのトークンの数。式の Debug の長さは位置などで膨らむ)
        let ntok = lexer::tokenize(a.src[it.start..it.end].as_bytes()).len();
        if ntok > acg::MAX_TOKENS {
            continue;
        }
        if let Ok(f) = fn_ast(a, id as u32) {
            let ok = acg::inline_candidate(&f);
            if std::env::var_os("ZUSTC_INLINE_LOG").is_some() {
                eprintln!("inline {} {ntok} {}", if ok { "yes" } else { "no " }, p.names[id]);
            }
            if ok {
                inl.bodies.insert(id as u32, f);
                if ii < p.prelude_from {
                    inl.user_lines.insert(id as u32, it.line as u32 - 1);
                }
            }
        }
    }
    inl
}

fn concrete(p: &Program, id: usize) -> bool {
    p.item_of[id].is_some() && !p.sigs[id].is_generic() && p.sigs[id].has_body
}

/// 子プロセスから受け取るコンパイル結果の並び: (失敗か, 番号, 中身)
fn read_all(fd: i32) -> Vec<u8> {
    let mut out = vec![];
    let mut buf = vec![0u8; 1 << 16];
    loop {
        // SAFETY: buf は buf.len() byte ある
        let n = unsafe { c_read(fd, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

fn write_all(fd: i32, mut d: &[u8]) {
    while !d.is_empty() {
        // SAFETY: d は d.len() byte ある
        let n = unsafe { c_write(fd, d.as_ptr(), d.len()) };
        if n <= 0 {
            return;
        }
        d = &d[n as usize..];
    }
}

impl Session2 {
    /// 関数 id のキャッシュのキー: 本体のハッシュ + 自分と参照先のシグネチャ + 名前で現れる struct (レイアウトごと) + const

    /// closed: prelude の関数で、ユーザの関数を呼びえないもの (総称でないか、型引数が prelude の型だけ)。参照の
    /// シグネチャは prelude のものだけを入れる (名前の最後で多めに取ると、ユーザの `Acc::push` などが入って、同じ prelude の
    /// 関数がクレートごとに違うキーになり、どのクレートでもコンパイルし直していた)
    fn fn_key_r(&self, a: &Analysis, id: usize, closed: Option<&[bool]>) -> Key {
        let ii = a.prog.item_of[id].unwrap() as usize;
        let pf = a.prog.prelude_from as usize;
        // 閉じた依存するクレートの関数 (と、型引数に依存するクレートの型を持つ prelude の関数): 本体・自分のシグネチャと、
        // 見えるクレートの中の参照先だけ (prelude はコンパイラで決まる、ほかのクレートのものは呼べない)
        if let Some(u) = closed.filter(|u| ii < pf || u.iter().any(|&x| x)) {
            let in_u = |item: Option<u32>| item.is_some_and(|ri| a.crate_of_item(ri as usize).is_some_and(|c| u[c]));
            let mut h = KeyHasher::new();
            h.add(&self.cid);
            h.add(&3u8);
            h.add(&(self.opts.opt as u8));
            h.add(&lint_bits(&a.prog));
            h.add(&(self.opts.backend_id()));
            h.add(&a.items[ii].hash);
            h.add(&a.sig_key(id as u32));
            for &r in &a.refs[id] {
                if in_u(a.prog.item_of[r as usize]) {
                    h.add(&a.sig_key(r));
                }
            }
            for &st in &a.srefs[id] {
                let b = a.prog.base_of(st);
                if in_u(a.prog.struct_item.get(b as usize).copied()) {
                    h.add(&a.prog.structs[st as usize].deep);
                }
            }
            for &c in &a.crefs[id] {
                let cd = &a.prog.consts[c as usize];
                if in_u(Some(cd.item)) {
                    h.add(&cd.name);
                    h.add(&a.items[cd.item as usize].hash);
                    a.prog.hash_ty(cd.ty, &mut h);
                }
            }
            match a.prog.panic_handler {
                Some(p) => h.add(&a.prog.names[p as usize]),
                None => h.add(&0u8),
            }
            return h.finish();
        }
        let closed = closed.is_some();
        // 閉じた prelude の関数: prelude の本体とシグネチャはコンパイラ (cid) に入っているので、本体と自分のシグネチャと
        // panic の行き先 (`#[panic_handler]`) だけで決まる (参照先を名前の最後で多めに取ると、クレートごとに選ぶ prelude が
        // 違うのでキーが変わり、同じ関数をクレートごとにコンパイルし直していた)
        if closed && ii >= pf {
            let mut h = KeyHasher::new();
            h.add(&self.cid);
            h.add(&2u8);
            h.add(&(self.opts.opt as u8));
            h.add(&lint_bits(&a.prog));
            h.add(&(self.opts.backend_id()));
            h.add(&a.items[ii].hash);
            h.add(&a.sig_key(id as u32));
            match a.prog.panic_handler {
                Some(p) => h.add(&a.prog.names[p as usize]),
                None => h.add(&0u8),
            }
            return h.finish();
        }
        let mut h = KeyHasher::new();
        h.add(&self.cid);
        h.add(&1u8);
        h.add(&(self.opts.opt as u8));
        h.add(&lint_bits(&a.prog));
        h.add(&(self.opts.backend_id()));
        h.add(&a.items[ii].hash);
        // (with traits of one name in two modules, which impl a method call takes depends on the traits in scope at the
        // item: a `use` changed elsewhere changes it, with the body the same)
        if let Some(vis) = a.prog.traits_at_item(ii as u32) {
            h.add(&vis.join(","));
        }
        if a.prog.closures.contains_key(&(id as u32)) {
            h.add(&a.prog.names[id]);
        }
        h.add(&a.sig_key(id as u32));
        for &r in &a.refs[id] {
            if closed && a.prog.item_of[r as usize].is_some_and(|ri| (ri as usize) < pf) {
                continue;
            }
            h.add(&a.sig_key(r));
            // (展開しうるユーザの関数は、本体と行も効く)
            if let Some(&lb) = a.inl.user_lines.get(&r) {
                if let Some(ri) = a.prog.item_of[r as usize] {
                    h.add(&a.items[ri as usize].hash);
                    h.add(&lb);
                    // (and the traits in scope there, which the inlined body's method calls resolve with; and those of
                    // the user fns inlined into it in turn, inlining going two deep)
                    if !a.prog.trait_scopes.is_empty() {
                        if let Some(vis) = a.prog.traits_at_item(ri) {
                            h.add(&vis.join(","));
                        }
                        for &r2 in &a.refs[r as usize] {
                            if a.inl.user_lines.contains_key(&r2) {
                                if let Some(vis) = a.prog.item_of[r2 as usize].and_then(|ri2| a.prog.traits_at_item(ri2)) {
                                    h.add(&vis.join(","));
                                }
                            }
                        }
                    }
                }
            }
        }
        for &st in &a.srefs[id] {
            if closed && !a.prog.is_prelude_ty(Ty::Struct(st)) {
                continue;
            }
            h.add(&a.prog.structs[st as usize].deep);
        }
        for &c in &a.crefs[id] {
            let cd = &a.prog.consts[c as usize];
            h.add(&cd.name);
            h.add(&a.items[cd.item as usize].hash);
            a.prog.hash_ty(cd.ty, &mut h);
        }
        h.finish()
    }

    /// 関数かインスタンスのキー (インスタンスは元の関数のキー + 型引数)
    fn key_of(&self, a: &Analysis, insts: &Mutex<Insts>, id: u32) -> Key {
        let g = insts.lock().unwrap();
        let body = g.body_of(id) as usize;
        let closed = a.closed_univ(&*g, id);
        let k = self.fn_key_r(a, body, closed.as_ref().map(|v| v.as_slice()));
        match g.get(id) {
            None => k,
            Some(inst) => {
                let mut h = KeyHasher::new();
                h.add(&k);
                h.add(&7u8);
                for &t in &inst.args {
                    a.prog.hash_ty(t, &mut h);
                }
                h.finish()
            }
        }
    }

}

/// An error at an item's first line about a name it does not find (`L:1: cannot find type \`X\``): at the line of the
/// item where X is first written instead (a field, a parameter), when that is another line
fn name_line(e: &str, src: &str, items: &[SrcItem]) -> String {
    let Some((l, rest)) = e.split_once(":1: ") else { return e.to_string() };
    let Ok(line) = l.parse::<usize>() else { return e.to_string() };
    let name = ["cannot find type `", "cannot find trait `", "cannot find value `", "cannot find struct `"]
        .iter()
        .find_map(|p| rest.strip_prefix(p))
        .and_then(|r| r.split('`').next());
    let (Some(name), Some(it)) = (name, items.iter().find(|it| it.line == line)) else { return e.to_string() };
    let text = &src[it.start..it.end.min(src.len())];
    let word = |i: usize| {
        let before = text[..i].chars().next_back().is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let after = text[i + name.len()..].chars().next().is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        before && after
    };
    match text.match_indices(name).map(|(i, _)| i).find(|&i| word(i)) {
        Some(i) => format!("{}:1: {rest}", line + text[..i].matches('\n').count()),
        None => e.to_string(),
    }
}

/// compile_id と、コード生成が要求した (総称関数の) インスタンス
fn compile_id_req(a: &Analysis, insts: &Mutex<Insts>, id: u32) -> R<(Blob, Vec<u32>)> {
    let body = insts.lock().unwrap().body_of(id);
    // (本体の構文木は本体ごとに 1 回だけ読む: 総称関数のインスタンスごとに読み直していた (コンパイルの 10%))
    let mut asts = a.asts.lock().unwrap();
    if !asts.contains_key(&body) {
        let f = fn_ast(a, body)?;
        asts.insert(body, f);
    }
    let f = asts.get(&body).unwrap();
    let ii = a.prog.item_of[body as usize].unwrap() as usize;
    let it = &a.items[ii];
    // (acg のエラーはファイルの中の `行:桁:` を持つ。持たないものはアイテムの先頭の行にする)
    let lb = {
        let g = insts.lock().unwrap();
        a.line_base_for(&*g, id).unwrap_or(it.line as u32 - 1)
    };
    // (a dependency crate's function has its lines from that crate's start, tagged (line_base_for): its errors are put
    // back at the lines of the combined source)
    let shift = {
        let g = insts.lock().unwrap();
        a.line_shift(&*g, id)
    };
    let (c, req) = acg::compile(&a.prog, insts, id, f, lb, &a.inl)
        .map_err(|e| if acg::has_loc(&e) { untag_lines(&e, shift) } else { format!("{}:1: in `{}`: {e}", it.line, a.prog.names[body as usize]) })?;
    // (check は機械語にしない。ビルドは記録をバックエンドで機械語にする)
    if a.inl.check_only {
        return Ok((acg::Compiled::empty_blob(), req));
    }
    lower_blob(a, c, req)
}

/// 記録を機械語にして Blob を作る (バックエンドなしでは作れない)
#[cfg(not(feature = "build"))]
fn lower_blob(_: &Analysis, _: acg::Compiled, _: Vec<u32>) -> R<(Blob, Vec<u32>)> { Err("internal: zust-core was built without the backend (feature `build`)".into()) }

/// 位置付きのエラー (行・桁は 1 始まり)。check / LSP が返す
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

impl Diagnostic {
    /// 内部の "L:C: msg" (か "L: msg") の形の文字列から作る
    pub fn parse(s: &str) -> Diagnostic {
        let b = s.as_bytes();
        let num = |from: usize| -> (usize, usize) {
            let mut i = from;
            while i < b.len() && b[i] == b' ' {
                i += 1;
            }
            let st = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            (s[st..i].parse().unwrap_or(0), i)
        };
        let (line, i) = num(0);
        if line > 0 && i < b.len() && b[i] == b':' {
            let (col, j) = num(i + 1);
            if col > 0 && j < b.len() && b[j] == b':' {
                return Diagnostic { line, col, msg: s[j + 1..].trim().to_string() };
            }
            return Diagnostic { line, col: 1, msg: s[i + 1..].trim().to_string() };
        }
        Diagnostic { line: 1, col: 1, msg: s.to_string() }
    }
}

impl Session2 {
    /// 型検査だけを行い、全診断を返す (`zustc check` と LSP)。到達しない関数も、総称関数は要求されたインスタンスごとに
    /// 検査する (コード生成までしてエラーを集め、結果はキャッシュに入れる: 次の検査・ビルドはそれを使う)。prelude の中の診断は出さない
    pub fn check(&mut self, src: &str) -> Vec<Diagnostic> {
        let user_lines = src.bytes().filter(|&c| c == b'\n').count() + 1;
        let mut a = match self.analyze(src) {
            Ok(a) => a,
            Err(e) => return vec![Diagnostic::parse(&e)],
        };
        // 検査だけ: コード生成 (acg) で型を検査し、機械語は作らない (ZUSTC_CHECK_CODEGEN=1 で機械語まで作る)。
        // 検査済みの印は、要求したインスタンスへの再配置だけを持つ空の Blob を、ビルドとは別のキーで覚える
        // (ビルドの Blob があればそれも検査済みとして使う)
        let check_only = std::env::var_os("ZUSTC_CHECK_CODEGEN").is_none();
        a.inl.check_only = check_only;
        let mut tm = Timer { on: std::env::var_os("ZUSTC_PROF").is_some(), last: now_ns(), start: now_ns() };
        let (mut n_keys, mut n_hit, mut n_checked, mut n_memo) = (0, 0, 0, 0);
        let insts = Mutex::new(Insts::new(&a.prog));
        let mut errs: Vec<String> = vec![];
        let mut done: FxSet<u32> = FxSet::default();
        // 前回の check の記録: 宣言の指紋が同じなら、本体のアイテムが変わっていない関数は検査済み (キーも読み戻しも要らない)。
        // 要求していたインスタンスは名前で辿り、本体が変わったものだけを要求し直す
        let old = match self.chk.take() {
            Some(m) if m.fp == a.iface_fp && check_only && std::env::var_os("ZUSTC_NO_CHECK_MEMO").is_none() => Some(m),
            _ => None,
        };
        // (記録を作るのは、続けて検査する (キャッシュを使う) セッションだけ: 1 度だけの `zustc check` では作らない)
        let keep = check_only && !self.opts.no_cache;
        let mut memo = ChkMemo { fp: a.iface_fp, ents: FxMap::default() };
        let mut seen: FxSet<String> = FxSet::default();
        let name_of = |g: &Insts, id: u32| -> String {
            match g.get(id) {
                Some(i) => format!("{}\u{1}{}", a.prog.names[i.base as usize], i.sig.symbol),
                None => a.prog.names[id as usize].clone(),
            }
        };
        let body_hash = |name: &str| -> Option<Key> {
            let &id = a.prog.index.get(name)?;
            let ii = a.prog.item_of[id as usize]?;
            Some(a.items[ii as usize].hash)
        };
        let n = a.prog.names.len();
        let mut wave: Vec<u32> = (0..n)
            .filter(|&i| concrete(&a.prog, i) && a.prog.item_of[i].is_some_and(|ii| ii < a.prog.prelude_from) && !a.is_ext(i as u32))
            .map(|i| i as u32)
            .collect();
        while !wave.is_empty() {
            for &id in &wave {
                if !done.insert(id) {
                    continue;
                }
                let (name, body) = {
                    let g = insts.lock().unwrap();
                    (name_of(&*g, id), a.prog.names[g.body_of(id) as usize].clone())
                };
                if !seen.insert(name.clone()) {
                    continue;
                }
                // (記録から: 本体が同じなら、要求していたインスタンスを名前で辿る)
                if let Some(o) = &old {
                    if let Some(e) = o.ents.get(&name) {
                        if e.ok && Some(e.body_hash) == body_hash(&e.body) {
                            n_memo += 1;
                            let mut st: Vec<String> = e.children.clone();
                            memo.ents.insert(name.clone(), e.clone());
                            while let Some(c) = st.pop() {
                                if seen.contains(&c) {
                                    continue;
                                }
                                let Some(ce) = o.ents.get(&c) else { continue };
                                if ce.ok && Some(ce.body_hash) == body_hash(&ce.body) {
                                    seen.insert(c.clone());
                                    n_memo += 1;
                                    st.extend(ce.children.iter().cloned());
                                    memo.ents.insert(c, ce.clone());
                                } else {
                                    // (本体が変わったインスタンス: 要求し直して検査する)
                                    let _ = crate::cache2::decode_blob(&ce.me, &a.prog, &insts);
                                }
                            }
                            continue;
                        }
                    }
                }
                let k = self.key_of(&a, &insts, id);
                let kc = k ^ CHECK_SALT;
                n_keys += 1;
                // (キャッシュにあれば検査済み: 読み戻すと要求していたインスタンスも要求し直す)
                let hit = match self.get(k).and_then(|d| crate::cache2::decode_blob(&d, &a.prog, &insts)) {
                    Some(b) => Some(b),
                    None if check_only => self.get(kc).and_then(|d| crate::cache2::decode_blob(&d, &a.prog, &insts)),
                    None => None,
                };
                let mut ok = true;
                let req: Option<Vec<u32>> = match hit {
                    Some(b) => {
                        n_hit += 1;
                        Some(b.relocs.iter().filter_map(|r| match r.target { acg::Target::Func(i) => Some(i), _ => None }).collect())
                    }
                    None => {
                        n_checked += 1;
                        match compile_id_req(&a, &insts, id) {
                            Ok((b, req)) => {
                                if !self.opts.no_cache {
                                    let b = if check_only {
                                        let relocs = req.iter().map(|&i| acg::BlobReloc { offset: 0, kind: 4, addend: 0, target: acg::Target::Func(i) }).collect();
                                        Blob { relocs, ..b }
                                    } else {
                                        b
                                    };
                                    let mut e = vec![];
                                    {
                                        let g = insts.lock().unwrap();
                                        crate::cache2::encode_blob(&b, &a.prog, &*g, &mut e);
                                    }
                                    self.put(if check_only { kc } else { k }, e);
                                }
                                Some(req)
                            }
                            Err(e) => {
                                errs.push(e);
                                ok = false;
                                Some(vec![])
                            }
                        }
                    }
                };
                // (記録)
                if !keep {
                    continue;
                }
                if let (Some(req), Some(bh)) = (req, body_hash(&body)) {
                    let g = insts.lock().unwrap();
                    let children: Vec<String> = req.iter().filter(|&&i| g.get(i).is_some()).map(|&i| name_of(&*g, i)).collect();
                    let mut me = vec![];
                    if g.get(id).is_some() {
                        let b = Blob { align: 1, code: vec![], relocs: vec![acg::BlobReloc { offset: 0, kind: 4, addend: 0, target: acg::Target::Func(id) }], datas: vec![], vtables: vec![], traps: vec![], line_base: None, code_ref: None };
                        crate::cache2::encode_blob(&b, &a.prog, &*g, &mut me);
                    }
                    memo.ents.insert(name, ChkEnt { body, body_hash: bh, children, me, ok });
                }
            }
            wave = std::mem::take(&mut insts.lock().unwrap().pending);
            wave.retain(|id| !done.contains(id));
        }
        if keep {
            self.chk = Some(memo);
        }
        tm.lap("check", format!("{n_memo} from the last check, {n_keys} keys, {n_hit} cached, {n_checked} checked"));
        let mut d: Vec<Diagnostic> = errs.iter().map(|e| Diagnostic::parse(e)).filter(|d| d.line <= user_lines).collect();
        d.sort_by(|x, y| (x.line, x.col).cmp(&(y.line, y.col)));
        d.dedup();
        d
    }
}

/// The lint levels codegen checks (`#![deny(unused_must_use)]`, `trivial_casts` ..) as one byte for the cache keys: a
/// function compiled before such a `#![deny]` was added is not reused without its check
fn lint_bits(p: &Program) -> u8 {
    p.deny_must_use as u8 | (p.deny_trivial_casts.0 as u8) << 1 | (p.deny_trivial_casts.1 as u8) << 2 | (p.deny_unreachable as u8) << 3 | (p.deny_unused_alloc as u8) << 4
}
