//! zustc の CLI。
//!   zustc <input.rs> [-o out] [...]   ビルド (自前のフロントエンド + Cranelift。`--backend=minclif` で minclif)
//!   zustc build <workspace> [-o dir]   ワークスペース (複数のクレート) をビルド
//!   zustc check <input.rs>            型検査だけ (全診断を表示)

use std::path::PathBuf;

use zustc::driver2::{Options2, Session2};

mod stamp;

fn usage() -> ! {
    eprintln!(
        "usage: zustc <input.rs> [-o out] [-O] [--backend=cranelift|minclif] [--target=wasm32-unknown-unknown|wasm32-wasip1|wasm32-wasip2] [--linker=NAME] [--features=a,b] [-L dir] [--time] [-j N] [--no-cache]\n       zustc build <workspace-file> [-o out-dir] [-O] [--backend=..] [-j N] [--time] [--no-cache]\n       zustc check <input.rs>"
    );
    std::process::exit(2)
}

fn die(msg: String) -> ! {
    // (a program's type renamed against the prelude's: its own name, zust_syntax::prelude::rename_user_collisions)
    eprintln!("error: {}", msg.replace("__u_", ""));
    std::process::exit(1)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = argv.iter().cloned().peekable();
    let mut o = Options2::default();
    let mut input = PathBuf::new();
    let mut output = PathBuf::new();
    let mut mode = "build";
    // (バックエンド: 既定は Cranelift。ZUSTC_BACKEND=minclif / --backend=minclif で minclif)
    let mut backend = std::env::var("ZUSTC_BACKEND").unwrap_or_else(|_| "cranelift".to_string());
    let mut target = String::from("x86_64-unknown-linux-gnu");
    match args.peek().map(String::as_str) {
        Some("check") => {
            mode = "check";
            args.next();
        }
        Some("build") => {
            mode = "workspace";
            args.next();
        }
        Some("lsp") => {
            eprintln!("zustc: LSP サーバは別のバイナリ zust-analyzer になった (cargo build --release -p zust-analyzer)");
            std::process::exit(2);
        }
        _ => {}
    }
    while let Some(a) = args.next() {
        match a.as_str() {
            "-o" => output = args.next().unwrap_or_else(|| usage()).into(),
            "-O" => o.opt = true,
            "--time" => o.time = true,
            "--no-cache" => o.no_cache = true,
            // `-L dir` / `-L native=dir`: where `#[link(name = "x")]` looks for libx.a
            "-L" => {
                let d = args.next().unwrap_or_else(|| usage());
                o.lib_dirs.push(d.strip_prefix("native=").or_else(|| d.strip_prefix("all=")).unwrap_or(&d).into());
            }
            "-j" => o.jobs = args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()),
            s if s.starts_with("--backend=") => backend = s["--backend=".len()..].to_string(),
            "--target" => target = args.next().unwrap_or_else(|| usage()),
            s if s.starts_with("--target=") => target = s["--target=".len()..].to_string(),
            s if s.starts_with("--linker=") => o.linker = Some(s["--linker=".len()..].into()),
            s if s.starts_with("--features=") => o.features.extend(s["--features=".len()..].split(',').filter(|f| !f.is_empty()).map(|f| f.to_string())),
            s if s.starts_with('-') => usage(),
            s => input = s.into(),
        }
    }
    if input.as_os_str().is_empty() {
        usage();
    }
    match target.as_str() {
        "x86_64-unknown-linux-gnu" => {}
        // (a wasm module: its own backend and writer, no linker)
        "wasm32-unknown-unknown" => {
            backend = "wasm".to_string();
            o.module_writer = Some(zustc::wasm_backend::write_module);
        }
        // (WASI: the same backend, a writer that imports wasi_snapshot_preview1 and synthesizes _start)
        "wasm32-wasip1" => {
            backend = "wasm".to_string();
            o.module_writer = Some(zustc::wasm_backend::write_module_wasip1);
            o.wasi = true;
        }
        "wasm32-wasip2" => {
            backend = "wasm".to_string();
            o.module_writer = Some(zustc::wasm_backend::write_module_wasip2);
            o.wasi = true;
        }
        t => {
            eprintln!("error: unknown target `{t}` (x86_64-unknown-linux-gnu / wasm32-unknown-unknown / wasm32-wasip1 / wasm32-wasip2)");
            std::process::exit(2);
        }
    }
    match backend.as_str() {
        // (the wasm lowerer only with the wasm writer: `--target wasm32-unknown-unknown`, never alone)
        "wasm" if o.module_writer.is_some() => o.backend = Some(zustc::wasm_backend::compile),
        "cranelift" => o.backend = Some(zustc::clif_backend::compile),
        "minclif" => o.backend = None,
        b => {
            eprintln!("error: unknown backend `{b}` (cranelift / minclif; wasm is chosen by --target wasm32-unknown-unknown)");
            std::process::exit(2);
        }
    }
    if mode == "workspace" {
        let crates = zustc::driver2::read_workspace(&input).unwrap_or_else(|e| die(e));
        let out_dir = if output.as_os_str().is_empty() { input.parent().map(|d| d.join("target")).unwrap_or("target".into()) } else { output };
        // (キャッシュはクレートごとに out_dir/<名前>.crcache2)
        match Session2::new(o).build_workspace(&crates, &out_dir) {
            Ok(_) => std::process::exit(0),
            Err(e) => die(e),
        }
    }
    if mode == "check" {
        // (`mod x;` のファイルも読む。診断の行は結合したソースのもの: 1 ファイルならそのまま)
        let (src, _) = zustc::srcmap::load_crate(&input, "src/main.rs", &o.cfg_with(&o.features)).unwrap_or_else(|e| die(e));
        let diags = Session2::new(Options2 { no_cache: true, jobs: 1, ..o }).check(&src);
        for d in &diags {
            eprintln!("error: {}:{}:{}: {}", input.display(), d.line, d.col, d.msg.replace("__u_", ""));
        }
        std::process::exit(if diags.is_empty() { 0 } else { 1 });
    }
    let output = if output.as_os_str().is_empty() { input.with_extension("") } else { output };
    let mut sp = output.as_os_str().to_owned();
    sp.push(".stamp");
    let sp = PathBuf::from(sp);
    let key = stamp::key(&argv);
    if !o.no_cache && std::env::var_os("ZUSTC_NO_STAMP").is_none() {
        if stamp::fresh(&sp, &key) {
            if o.time {
                eprintln!("  fresh (no stamp moved)");
            }
            return;
        }
        let _ = std::fs::remove_file(&sp);
    }
    let no_cache = o.no_cache;
    let mut s = Session2::new(o);
    let mut cp = output.as_os_str().to_owned();
    cp.push(".crcache2");
    let cp = PathBuf::from(cp);
    s.load_cache(&cp);
    match s.build_path(&input, "src/main.rs", &output) {
        Ok(_) => {
            if let Err(e) = s.save_cache(&cp) {
                eprintln!("warning: failed to write cache: {e}");
            }
            if let Some(m) = s.last_map.as_ref().filter(|m| !m.volatile && !no_cache) {
                stamp::save(&sp, &key, &m.inputs, &m.probes, &output);
            }
        }
        Err(e) => die(format!("{}: {e}", input.display())),
    }
}
