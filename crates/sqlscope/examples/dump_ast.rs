fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dialect: polyglot_sql::DialectType = args[1].parse().unwrap();
    let ast = polyglot_sql::parse(&args[2], dialect).unwrap();
    println!("{}", serde_json::to_string_pretty(&ast).unwrap());
}
