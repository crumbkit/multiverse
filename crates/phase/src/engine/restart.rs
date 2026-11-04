//! The restart runner, which carries out a process's
//! [`Restart`](crate::Restart) policy with exponential backoff and a
//! maximum count, and resolves the environment again as its
//! [`Resolve`](crate::Resolve) says.
