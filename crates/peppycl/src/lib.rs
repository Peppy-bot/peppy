/// A simple hello world function for the peppycl library.
///
/// # Examples
///
/// ```
/// use peppycl::hello;
///
/// let message = hello();
/// assert_eq!(message, "Hello, world from peppycl!");
/// ```
pub fn hello() -> &'static str {
    "Hello, world from peppycl!"
}

/// Returns a personalized greeting.
///
/// # Arguments
///
/// * `name` - The name to include in the greeting
///
/// # Examples
///
/// ```
/// use peppycl::greet;
///
/// let message = greet("Alice");
/// assert_eq!(message, "Hello, Alice!");
/// ```
pub fn greet(name: &str) -> String {
    format!("Hello, {}!", name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hello() {
        assert_eq!(hello(), "Hello, world from peppycl!");
    }

    #[test]
    fn test_greet() {
        assert_eq!(greet("Bob"), "Hello, Bob!");
        assert_eq!(greet(""), "Hello, !");
    }
}
