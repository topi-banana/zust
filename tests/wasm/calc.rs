// wasm32-unknown-unknown 用: import のない計算だけの no_std ライブラリ。main は不要
#![crate_type="lib"]
#![no_std]
#![no_main]
use core::panic::PanicInfo;
#[panic_handler]
fn panic(_i: &PanicInfo) -> ! { loop {} }

#[unsafe(no_mangle)]
pub extern "C" fn fib(n: i32) -> i64 {
    let mut a: i64 = 0;
    let mut b: i64 = 1;
    let mut i = 0;
    while i < n { let t = a + b; a = b; b = t; i += 1; }
    a
}

#[unsafe(no_mangle)]
pub extern "C" fn sum_to(n: i64) -> i64 {
    let mut s: i64 = 0;
    let mut i: i64 = 1;
    while i <= n { s += i; i += 1; }
    s
}

#[unsafe(no_mangle)]
pub extern "C" fn mix(a: i64, b: i64) -> i64 {
    (a.min(b) + a.max(b)) * 2 - (a ^ b)
}
