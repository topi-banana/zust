// 内部ランタイム shim (malloc / memset / memcpy / memmove) とポインタ算術 (I64 + I32) を確認する
#![no_std]
#![no_main]
use core::panic::PanicInfo;
unsafe extern "C" {
    fn putchar(c: i32) -> i32;
    fn malloc(n: usize) -> *mut u8;
    fn free(p: *mut u8);
    fn memset(p: *mut u8, c: i32, n: usize) -> *mut u8;
    fn memcpy(d: *mut u8, s: *const u8, n: usize) -> *mut u8;
    fn memmove(d: *mut u8, s: *const u8, n: usize) -> *mut u8;
}
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
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe {
        let p = malloc(16);
        memset(p, 0, 16);
        for i in 0..16 { *p.add(i) = (i as u8) * 2; }
        print_u64(*p.add(7) as u64); // 14
        let q = malloc(16);
        memcpy(q, p as *const u8, 16);
        print_u64(*q.add(15) as u64); // 30
        memmove(q.add(2), q as *const u8, 14); // 重なりを後ろへ
        print_u64(*q.add(2) as u64); // 0
        print_u64(*q.add(15) as u64); // 26
        free(p);
        free(q);
    }
    0
}
