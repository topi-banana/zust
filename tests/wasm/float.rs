// wasm の浮動小数点 emit 確認用。浮動小数点リテラルは strtod が要るので整数から作る。
// 表示も strtod が要るので整数にキャストして出す。
fn main() {
    // int -> float (i32/i64, signed) と四則演算
    let a = (7 as f64) / (2 as f64); // 3.5
    let b = (3 as f64) * (4 as f64) - (2 as f64); // 10
    println!("{} {}", (a * (2 as f64)) as i64, b as i64); // 7 10
    let c = (-7 as f64) / (2 as f64); // -3.5
    println!("{} {}", c as i64, (c + a) as i64); // -3 0
    // i64 -> f64 (大きい値) と f64 -> i64
    let big = (10 as f64).powi(10) + (1 as f64);
    println!("{}", big as i64); // 10000000001
    // f32 の四則と i32 への変換
    let x = (7 as f32) / (2 as f32); // 3.5
    let y = (x + (1 as f32)) * (2 as f32); // 9
    println!("{} {}", x as i32, y as i32); // 3 9
    // promote / demote
    let p = x as f64;
    let q = (p * (2 as f64)) as f32;
    println!("{}", q as i32); // 7
    // floor / ceil / trunc / abs / sqrt
    println!("{} {} {} {}", a.floor() as i64, a.ceil() as i64, c.trunc() as i64, c.abs() as i64); // 3 4 -3 3
    println!("{}", ((9 as f64).sqrt()) as i64); // 3
    let s32 = (16 as f32).sqrt();
    println!("{}", s32 as i32); // 4
    // round / roundf: 0.5 境界は 0 から遠い方向 (wasm nearest は銀行家丸めなので shim で補正)
    let half = (1 as f64) / (2 as f64);
    println!("{} {} {} {}", half.round() as i64, (-half).round() as i64, ((2 as f64) + half).round() as i64, (-((2 as f64) + half)).round() as i64); // 1 -1 3 -3
    let h32 = (1 as f32) / (2 as f32);
    println!("{} {} {} {}", h32.round() as i32, (-h32).round() as i32, ((2 as f32) + h32).round() as i32, (-((2 as f32) + h32)).round() as i32); // 1 -1 3 -3
    // round は大きな値で恒等
    println!("{}", (10 as f64).powi(16).round() as i64); // 10011111111100000
    // 比較
    let t = a > (3 as f64);
    let u = c < (0 as f64);
    println!("{} {}", t as i32, u as i32); // 1 1
    println!("{}", (a == (7 as f64) / (2 as f64)) as i32); // 1
}
