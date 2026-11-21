//! wasm ターゲット (wasm32-unknown-unknown / wasm32-wasip1 / wasm32-wasip2) でビルドして wasmtime で動かす。
//! wasmtime が無ければスキップする。tests/wasm/ の *.rs はホストの driver2 テストの対象外 (examples/ ではない)。
//! upstream の tests/wasm.rs (Node サンドボックス) とは別に、wasmtime で WASI / unknown-unknown を確認する。
use std::path::{Path, PathBuf};
use std::process::Command;

fn wasmtime() -> Option<PathBuf> {
    let out = Command::new("sh").arg("-c").arg("command -v wasmtime").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() { None } else { Some(PathBuf::from(p)) }
}

fn tmpdir(prefix: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let d = std::env::temp_dir().join(format!("{prefix}_{}_{t}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// ターゲットごとの Options2 を作ってビルドする (出力は out そのもの。upstream の wasm writer は拡張子を付けない)
fn build(src: &str, out: &Path, target: &str, opt: bool) {
    let mut o = zustc::driver2::Options2 { opt, no_cache: true, jobs: 1, ..Default::default() };
    o.backend = Some(zustc::wasm_backend::compile);
    match target {
        "wasm32-unknown-unknown" => o.module_writer = Some(zustc::wasm_backend::write_module),
        "wasm32-wasip1" => {
            o.module_writer = Some(zustc::wasm_backend::write_module_wasip1);
            o.wasi = true;
        }
        "wasm32-wasip2" => {
            o.module_writer = Some(zustc::wasm_backend::write_module_wasip2);
            o.wasi = true;
        }
        t => panic!("unknown target {t}"),
    }
    zustc::driver2::Session2::new(o).build(src, out).unwrap_or_else(|e| panic!("{target} opt={opt}: {e}"));
}

fn run_wasi(wt: &PathBuf, target: &str, opt: bool) {
    let dir = tmpdir("zustwasm");
    let src = std::fs::read_to_string("tests/wasm/wasi_hello.rs").unwrap();
    let out = dir.join("hello");
    build(&src, &out, target, opt);
    assert!(out.exists(), "{target}: no module written");
    let a = Command::new(wt).arg(&out).output().unwrap();
    let stdout = String::from_utf8_lossy(&a.stdout).to_string();
    assert_eq!(stdout, "hello wasm\n50653\n4090\n3\n1335831723\nwasi\nwasm32\n", "{target} opt={opt}: stdout differs");
    assert_eq!(a.status.code().unwrap_or(-1), 42, "{target} opt={opt}: exit code");
    let _ = std::fs::remove_dir_all(&dir);
}

/// tests/wasm/ のソースを wasip1 でビルドして実行し、stdout と終了コードを見る
fn run_wasi_src(wt: &PathBuf, name: &str, want: &str, want_code: i32, opt: bool) {
    let dir = tmpdir("zustwasm");
    let src = std::fs::read_to_string(format!("tests/wasm/{name}.rs")).unwrap();
    let out = dir.join(name);
    build(&src, &out, "wasm32-wasip1", opt);
    let a = Command::new(wt).arg(&out).output().unwrap();
    let stdout = String::from_utf8_lossy(&a.stdout).to_string();
    assert_eq!(stdout, want, "{name} opt={opt}: stdout differs");
    assert_eq!(a.status.code().unwrap_or(-1), want_code, "{name} opt={opt}: exit code");
    let _ = std::fs::remove_dir_all(&dir);
}

fn invoke(wt: &PathBuf, wasm: &PathBuf, f: &str, args: &[&str]) -> String {
    let mut c = Command::new(wt);
    c.arg("--invoke").arg(f).arg(wasm);
    for a in args {
        c.arg(a);
    }
    let out = c.output().unwrap();
    assert!(out.status.success(), "--invoke {f}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn run_unknown(wt: &PathBuf, opt: bool) {
    let dir = tmpdir("zustwasm");
    let src = std::fs::read_to_string("tests/wasm/calc.rs").unwrap();
    let out = dir.join("calc");
    build(&src, &out, "wasm32-unknown-unknown", opt);
    assert_eq!(invoke(wt, &out, "fib", &["10"]), "55", "opt={opt}");
    assert_eq!(invoke(wt, &out, "fib", &["20"]), "6765", "opt={opt}");
    assert_eq!(invoke(wt, &out, "sum_to", &["100"]), "5050", "opt={opt}");
    assert_eq!(invoke(wt, &out, "mix", &["7", "3"]), "16", "opt={opt}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wasm_targets_run() {
    let Some(wt) = wasmtime() else {
        eprintln!("wasmtime not found; skipping wasm target test");
        return;
    };
    for opt in [false, true] {
        run_wasi(&wt, "wasm32-wasip1", opt);
        run_wasi(&wt, "wasm32-wasip2", opt);
        run_unknown(&wt, opt);
        // 関数ポインタ (call_indirect) と内部ランタイム shim (malloc/memset/memcpy/memmove)
        run_wasi_src(&wt, "fnptr", "13\n42\n15\n49\n140\n", 0, opt);
        run_wasi_src(&wt, "heap", "14\n30\n0\n26\n", 0, opt);
        // 構造体 / タプルの値返し (sret) と Option::take / enum match
        run_wasi_src(&wt, "sret", "14\n0\n42\n8\n15\n", 0, opt);
        // 浮動小数点: int↔float 変換・四則・floor/ceil/trunc/abs/sqrt・round (0.5 境界)
        run_wasi_src(
            &wt,
            "float",
            "7 10\n-3 0\n10000000001\n3 9\n7\n3 4 -3 3\n3\n4\n1 -1 3 -3\n1 -1 3 -3\n10000000000000000\n1 1\n1\n",
            0,
            opt,
        );
    }
}

/// std の examples を wasm でビルドして動かし、ホストの期待値 (tests/expected) と stdout・終了コードが同じか。
/// std ランタイム (realloc / sigaction / fflush / trap など) の shim と、sret・条件・overflow の emit を守る
fn run_std(wt: &PathBuf, name: &str, target: &str, opt: bool) {
    let dir = tmpdir("zustwasm");
    let src = std::fs::read_to_string(format!("examples/{name}.rs")).unwrap();
    let out = dir.join(name);
    build(&src, &out, target, opt);
    let a = Command::new(wt).arg(&out).output().unwrap();
    let stdout = String::from_utf8_lossy(&a.stdout).to_string();
    let want = std::fs::read_to_string(format!("tests/expected/examples.{name}.stdout")).unwrap();
    assert_eq!(stdout, want, "{name} {target} opt={opt}: stdout differs");
    let want_code: i32 = std::fs::read_to_string(format!("tests/expected/examples.{name}.code")).unwrap().trim().parse().unwrap();
    assert_eq!(a.status.code().unwrap_or(-1), want_code, "{name} {target} opt={opt}: exit code");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wasm_std_examples() {
    let Some(wt) = wasmtime() else {
        eprintln!("wasmtime not found; skipping wasm std example test");
        return;
    };
    // enum / struct / trait / Vec / String / hashmap / closure / 関数ポインタ / drop など std の代表例
    let names = ["enums", "structs", "methods", "options", "ordering", "vec_string", "iterators", "collections", "hashmap", "sorting", "traits", "closures", "dyn_trait", "fn_ptr_box", "drop"];
    for opt in [false, true] {
        for name in names {
            run_std(&wt, name, "wasm32-wasip1", opt);
        }
        // wasip2 も同じコアモジュールなので代表例で確認
        for name in ["enums", "vec_string", "hashmap", "dyn_trait"] {
            run_std(&wt, name, "wasm32-wasip2", opt);
        }
    }
}
