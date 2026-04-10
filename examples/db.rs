use lsm::{Db, LookupResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Db::open();

    println!("db opened");
    println!("is empty: {}", db.is_empty()?);

    db.put("user:1", "Alice")?;
    db.put("user:2", "Bob")?;

    println!("entry count: {}", db.len()?);
    println!("used bytes: {}", db.used_bytes()?);
    println!("remaining bytes: {}", db.remaining_bytes()?);

    match db.get("user:1")? {
        LookupResult::Value(value) => println!("user:1 => {value}"),
        LookupResult::NotFound => println!("user:1 is missing"),
    }

    db.delete("user:2")?;

    match db.get("user:2")? {
        LookupResult::Value(value) => println!("user:2 => {value}"),
        LookupResult::NotFound => println!("user:2 was deleted"),
    }

    db.close()?;
    println!("db closed");

    Ok(())
}
