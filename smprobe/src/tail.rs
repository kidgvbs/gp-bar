//! Last-N-lines tail of a text buffer (the sampler log).

/// The last `n` lines of `content`, in order. A trailing newline does not
/// produce an empty final line; `n` larger than the file returns everything.
pub fn last_lines(content: &str, n: usize) -> Vec<&str> {
  if n == 0 {
    return Vec::new();
  }
  let mut lines: Vec<&str> = content.lines().collect();
  if lines.len() > n {
    lines.drain(..lines.len() - n);
  }
  lines
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn returns_the_last_n_lines_in_order() {
    let content = "one\ntwo\nthree\nfour\n";
    assert_eq!(last_lines(content, 2), vec!["three", "four"]);
    assert_eq!(last_lines(content, 3), vec!["two", "three", "four"]);
  }

  #[test]
  fn fewer_lines_than_requested_returns_all_of_them() {
    let content = "one\n";
    assert_eq!(last_lines(content, 50), vec!["one"]);
  }

  #[test]
  fn empty_input_and_zero_n() {
    assert!(last_lines("", 10).is_empty());
    assert!(last_lines("one\ntwo", 0).is_empty());
  }

  #[test]
  fn no_trailing_newline_still_tails_the_last_line() {
    assert_eq!(last_lines("a\nb\nc", 1), vec!["c"]);
  }
}
