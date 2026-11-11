//! CJK-friendly emphasis (vendored pulldown-cmark patch): bold next to CJK
//! punctuation, as models write it, without spaces around it.

use omni_markdown::rich::{RichLimits, markdown_to_blocks};

fn bold(s: &str) -> bool {
    format!("{:?}", markdown_to_blocks(s, 0, RichLimits::TELEGRAM)).contains("Bold")
}

#[test]
fn bold_next_to_cjk_punctuation() {
    for s in [
        "这是**重要**的内容",
        "**注意：**这是内容",
        "这是**“引用”**的内容",
        "结尾**粗体。**后面",
        "（**括号**）",
        "価格は**「安い」**です",
        "이것은**“중요”**합니다",
        "删除~~“旧的”~~内容",
    ] {
        let want_strike = s.contains("~~");
        let dbg = format!("{:?}", markdown_to_blocks(s, 0, RichLimits::TELEGRAM));
        assert!(if want_strike { dbg.contains("Strike") } else { bold(s) }, "{s}: {dbg}");
    }
}

#[test]
fn other_rules_unchanged() {
    assert!(!bold(r"这是\*\*转义\*\*的"), "escaped");
    assert!(!bold("a ** b ** c"), "spaces around");
    assert!(!bold("中文 ** 空格 ** 中文"), "spaces around in CJK");
    assert!(bold("英文**bold**中文") && bold("**foo**bar"));
}
