use std::path::PathBuf;

#[must_use]
pub fn relative_base(eval_dir: Option<PathBuf>) -> Option<PathBuf> {
    eval_dir.or_else(|| std::env::current_dir().ok())
}

#[must_use]
pub fn expand_home(text: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match text.strip_prefix('~') {
        Some(rest) => format!("{home}{rest}"),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_path_starts_at_home_and_a_relative_one_at_the_eval_dir_else_the_cwd() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(expand_home("~/dir/file"), format!("{home}/dir/file"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("/abs/~/x"), "/abs/~/x");
        assert_eq!(relative_base(Some(PathBuf::from("/e"))), Some(PathBuf::from("/e")));
        assert_eq!(relative_base(None), std::env::current_dir().ok());
    }
}
