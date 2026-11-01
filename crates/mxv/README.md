# mxv

mxv is the command of [multiverse](../multiverse). It creates task
workspaces that span several git repositories, with one git worktree for
each repository, and archives and restores them.

mxv is under construction. It declares its workspace verbs, and each one
fails with `mxv::command::unbuilt` until the pull request that builds it.
`mxv commands list`, `mxv schemas list`, `mxv schemas show`, `mxv self usage`
and `mxv self figures list` work now.
