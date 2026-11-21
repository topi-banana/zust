// 関数ポインタ (call_indirect + funcref table) と配列のムーブを確認する
#![no_std]
#![no_main]
use core::panic::PanicInfo;
unsafe extern "C" { fn putchar(c: i32) -> i32; }
#[panic_handler]
fn panic(_i: &PanicInfo) -> ! { loop {} }
fn put(c: u8) { unsafe { putchar(c as i32); } }
fn print_u64(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if n == 0 { put(b'0'); put(b'\n'); return; }
    while n > 0 { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; }
    for &b in &buf[i..] { put(b); }
    put(b'\n');
}
fn add(a: i64, b: i64) -> i64 { a + b }
fn mul(a: i64, b: i64) -> i64 { a * b }
fn apply(f: fn(i64, i64) -> i64, a: i64, b: i64) -> i64 { f(a, b) }
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    print_u64(apply(add, 6, 7) as u64); // 13
    print_u64(apply(mul, 6, 7) as u64); // 42
    let fp: fn(i64, i64) -> i64 = if 3 > 2 { add } else { mul };
    print_u64(fp(10, 5) as u64); // 15
    let mut v: [u64; 8] = [0; 8];
    for i in 0..8 { v[i] = (i as u64) * (i as u64); }
    let b = v;
    print_u64(b[7]); // 49
    let mut sum = 0u64;
    for x in b.iter() { sum += x; }
    print_u64(sum); // 140
    0
}
