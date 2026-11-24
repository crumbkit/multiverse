//! The brace rule that a process's `env` value and a service's `env` value
//! share. Such a value writes a literal brace as `{{` or `}}`, and marks a
//! placeholder with single braces, such as `{port.api}`. Each kind of value
//! decides which placeholders it accepts.

#[cfg(test)]
mod tests;

/// One piece of a value that [`split`] reads.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Piece<P> {
    /// Literal text, with each doubled brace read as one brace.
    Text(String),
    /// A placeholder, as the caller's closure read it.
    Placeholder(P),
}

/// The text of a value from its first bad brace to its end. A bad brace is
/// a `{` or `}` that is neither doubled nor part of a placeholder that the
/// caller accepts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BadBrace<'text>(pub(crate) &'text str);

/// Splits `text` into literal text and placeholders. A `{{` or `}}` is one
/// literal brace. Any other `{` opens a placeholder that ends at the next
/// `}`, and `placeholder` reads the text between the two braces, or returns
/// `None` to refuse it. Adjacent literal text is joined into one piece, and
/// empty text has no pieces.
///
/// # Errors
///
/// Returns the text from the first bad brace to the end of `text`: a lone
/// `}`, a `{` with no `}` after it, or a placeholder that `placeholder`
/// refuses.
pub(crate) fn split<P>(
    text: &str,
    placeholder: impl Fn(&str) -> Option<P>,
) -> Result<Vec<Piece<P>>, BadBrace<'_>> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(['{', '}']) {
        let (before, tail) = rest.split_at(at);
        literal.push_str(before);
        if let Some(after) = tail.strip_prefix("{{") {
            literal.push('{');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("}}") {
            literal.push('}');
            rest = after;
        } else {
            let (read, after) = tail
                .strip_prefix('{')
                .and_then(|inside| {
                    let (name, after) = inside.split_once('}')?;
                    Some((placeholder(name)?, after))
                })
                .ok_or(BadBrace(tail))?;
            if !literal.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut literal)));
            }
            pieces.push(Piece::Placeholder(read));
            rest = after;
        }
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}
