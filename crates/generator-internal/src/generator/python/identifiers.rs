use crate::generator::naming::sanitize_component;

pub(crate) fn sanitize_python_identifier(raw: &str) -> String {
    let mut ident = sanitize_component(raw);
    if is_python_keyword(&ident) {
        ident.push('_');
    }
    ident
}

pub(crate) fn is_python_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_keywords_are_suffixed() {
        assert_eq!(sanitize_python_identifier("class"), "class_");
        assert_eq!(sanitize_python_identifier("from"), "from_");
        assert_eq!(sanitize_python_identifier("yield"), "yield_");
    }

    #[test]
    fn non_keywords_are_unchanged() {
        assert_eq!(sanitize_python_identifier("frame_id"), "frame_id");
        assert_eq!(sanitize_python_identifier("video-stream"), "video_stream");
    }
}
