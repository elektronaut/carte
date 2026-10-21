//! Recognizing a process that stopped at a debugger prompt from its output.

/// The prompt a line starts with when it is a Ruby debugger waiting for input:
/// `(rdbg)`, `(byebug)`, `pry(…)>` or `irb(…)>`, with what was typed after it.
/// Continuation prompts (`pry(main)*`, `irb(main):002*`), debug's prompts after a
/// fork (`(rdbg@puma#123)`) and IRB as debug's console (`irb:rdbg(main):002>`)
/// count too.
pub fn prompt(line: &str) -> Option<&str> {
    let line = line.trim_start();
    for fixed in ["(rdbg)", "(byebug)"] {
        if line.starts_with(fixed) {
            return Some(fixed);
        }
    }
    for open in ["(rdbg:", "(rdbg@"] {
        if let Some(rest) = line.strip_prefix(open)
            && let Some(end) = rest
                .find(") ")
                .or_else(|| rest.ends_with(')').then(|| rest.len() - 1))
        {
            return Some(&line[..open.len() + end + 1]);
        }
    }
    let start = match line.strip_prefix('[') {
        Some(rest) => {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || !rest[digits..].starts_with("] ") {
                return None;
            }
            digits + 3
        }
        None => 0,
    };
    let repl = &line[start..];
    if !["pry(", "irb(", "irb:rdbg("]
        .iter()
        .any(|open| repl.starts_with(open))
    {
        return None;
    }
    repl.match_indices(')').find_map(|(close, _)| {
        let tail = &repl[close + 1..];
        let mut rest = tail;
        while let Some(more) = rest.strip_prefix(':') {
            let digits = more.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            rest = &more[digits..];
        }
        let marker = rest.chars().next().filter(|c| matches!(c, '>' | '*'))?;
        let after = &rest[marker.len_utf8()..];
        (after.is_empty() || after.starts_with(' '))
            .then(|| &line[..start + close + 1 + (tail.len() - rest.len()) + 1])
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn recognizes_ruby_debugger_prompts() {
        for (line, found) in [
            ("(rdbg) ", "(rdbg)"),
            ("(rdbg) c", "(rdbg)"),
            ("(rdbg:remote) ", "(rdbg:remote)"),
            ("(byebug) ", "(byebug)"),
            ("[1] pry(main)> ", "[1] pry(main)>"),
            (
                "[12] pry(#<OrdersController>)> params",
                "[12] pry(#<OrdersController>)>",
            ),
            (
                "pry(#<Order(id: integer)>)> ",
                "pry(#<Order(id: integer)>)>",
            ),
            ("[3] pry(main)* ", "[3] pry(main)*"),
            ("irb(main):001:0> ", "irb(main):001:0>"),
            (
                "irb(#<OrdersController>):002> ",
                "irb(#<OrdersController>):002>",
            ),
            ("irb(main):003* ", "irb(main):003*"),
            ("irb(main)>", "irb(main)>"),
            ("irb:rdbg(#<Foo>):002> ", "irb:rdbg(#<Foo>):002>"),
            (
                "(rdbg@puma: cluster worker 0#4711) ",
                "(rdbg@puma: cluster worker 0#4711)",
            ),
            ("(rdbg@bin/rails (x)#12) n", "(rdbg@bin/rails (x)#12)"),
        ] {
            assert_eq!(prompt(line), Some(found), "{line:?}");
        }
    }

    #[test]
    fn ignores_lines_that_only_mention_one() {
        for line in [
            "",
            "Started GET \"/\" for ::1",
            "  (rdbg is not running)",
            "pry(main)",
            "irb(main):x> ",
            "calling irb(main)> later",
            "[x] pry(main)> ",
            "pry(main)>>= 1",
            "Processing by OrdersController#show",
        ] {
            assert_eq!(prompt(line), None, "{line:?}");
        }
    }

    proptest! {
        #[test]
        fn prompts_are_a_prefix_of_the_line(
            line in "[ \\[\\]0-9]{0,4}(pry|irb|irb:rdbg|\\(rdbg|\\(byebug)?[():@#>*a-z0-9 ]{0,12}|\\PC{0,16}",
        ) {
            if let Some(found) = prompt(&line) {
                prop_assert!(line.trim_start().starts_with(found), "{found:?}");
            }
        }
    }
}
