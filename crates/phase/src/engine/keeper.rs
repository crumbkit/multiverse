//! The keeper, which the hidden `phase keep` subcommand runs. A keeper is
//! the parent of exactly one app process. It holds only that process's
//! environment, applies the sandbox, gives the process a log file
//! descriptor, restarts it under its policy, and exits when it is stopped.
