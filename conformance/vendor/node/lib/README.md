# Placeholder for Node's `lib/`

Node's repository has a `lib/` directory (its JavaScript core). A few tests
`chdir` into it purely because it is a directory that reliably has no `.env`
file in it -- see `test/parallel/test-process-load-env-file.js`, which
explains the reasoning in a comment.

oam vendors Node's `test/` tree, not `lib/`, so the directory is recreated
here empty. The tests that use it only need somewhere to stand.
