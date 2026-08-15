//! `config check`: load, validate, print (secrets redacted).

use crate::config;

pub fn check(environment: &str) -> anyhow::Result<()> {
    let config = config::load(environment)?;
    println!("configuration OK (environment: {environment})");
    println!("{config:#?}");
    Ok(())
}
