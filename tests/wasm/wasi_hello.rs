// wasm32-wasip1 / wasip2 用: putchar で stdout に出す no_std プログラム
#![no_std]
#![no_main]
use core::panic::PanicInfo;
unsafe extern "C" { fn putchar(c: i32) -> i32; }
#[panic_handler]
fn panic(_i: &PanicInfo) -> ! { loop {} }
fn put(c: u8) { unsafe { putchar(c as i32); } }
fn puts(s: &[u8]) { for &b in s { put(b); } }
fn print_u64(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if n == 0 { put(b'0'); put(b'\n'); return; }
    while n > 0 { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; }
    for &b in &buf[i..] { put(b); }
    put(b'\n');
}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    puts(b"hello wasm\n");
    print_u64(37u32.pow(3) as u64); // 50653
    print_u64(409u32.checked_mul(10).unwrap_or(0) as u64); // 4090
    print_u64(7i64.min(3).max(-2) as u64); // 3
    let mut h: u32 = 0x811c9dc5;
    for &b in b"hello" { h ^= b as u32; h = h.wrapping_mul(16777619); }
    print_u64(h as u64); // 1335831723
    #[cfg(target_os = "wasi")]
    puts(b"wasi\n");
    #[cfg(target_arch = "wasm32")]
    puts(b"wasm32\n");
    42
}
