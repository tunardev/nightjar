# Contributing

Bug reports and patches are welcome.

Run `make check` before you open a pull request. It runs formatting, clippy, the tests and
`cargo deny`, which is what CI runs too.

Commits are conventional and one line:

```
fix(store): never prune a success whose after-children are still owed
```

By contributing you agree your work is published under the [MIT license](LICENSE).
