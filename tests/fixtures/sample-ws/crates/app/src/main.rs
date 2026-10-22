fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("{}", sample_util::describe(&args.join(" ")));
}
