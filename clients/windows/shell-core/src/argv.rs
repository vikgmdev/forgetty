//! Splitting a command line from the config into an argument list.
//!
//! `shell = "pwsh.exe -NoLogo"` and a profile's `command = "ssh user@host"` are one string in
//! the file (as in GTK) and an argument list for the process. Rules:
//! - Arguments are separated by white space.
//! - `"..."` and `'...'` group white space into one argument. The quotes are dropped.
//! - A backslash is a plain character, so `C:\Program Files\...` needs no doubling. The one
//!   exception is `\"` inside double quotes, which is a quote character.
//! - A quote that is never closed is an error, not a guess.

/// Split `s` into arguments. An empty or blank string gives an empty list.
pub fn split(s: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    // An empty pair of quotes is still an argument ("").
    let mut have = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' && chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    have = true;
                }
                c if c.is_whitespace() => {
                    if have || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        have = false;
                    }
                }
                c => {
                    cur.push(c);
                    have = true;
                }
            },
        }
    }
    if let Some(q) = quote {
        return Err(format!("a {q} quote is not closed"));
    }
    if have || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Vec<String> {
        split(x).unwrap()
    }

    #[test]
    fn plain_words() {
        assert_eq!(s("pwsh.exe -NoLogo  -NoProfile"), ["pwsh.exe", "-NoLogo", "-NoProfile"]);
        assert_eq!(s("  ssh user@host \t"), ["ssh", "user@host"]);
        assert!(s("").is_empty() && s("   ").is_empty());
    }

    #[test]
    fn quotes_group_and_backslashes_stay() {
        assert_eq!(
            s(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#),
            [r"C:\Program Files\PowerShell\7\pwsh.exe", "-NoLogo"]
        );
        assert_eq!(s("cmd /k 'echo hi there'"), ["cmd", "/k", "echo hi there"]);
        assert_eq!(s(r"C:\Windows\System32\cmd.exe"), [r"C:\Windows\System32\cmd.exe"]);
        // An escaped quote inside double quotes, a single quote inside double quotes.
        assert_eq!(s(r#"echo "say \"hi\"" "it's""#), ["echo", r#"say "hi""#, "it's"]);
        // Pieces glue together: a"b c"d is one argument.
        assert_eq!(s(r#"a"b c"d"#), ["ab cd"]);
        // An empty pair of quotes is an empty argument.
        assert_eq!(s(r#"x "" y"#), ["x", "", "y"]);
    }

    #[test]
    fn a_quote_left_open_is_an_error() {
        assert!(split(r#"pwsh "C:\Program Files"#).is_err());
        assert!(split("echo 'oops").is_err());
    }
}
