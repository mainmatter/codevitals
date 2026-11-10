git init -q -b main . && git config user.email t@t && git config user.name t
export GIT_AUTHOR_DATE="2026-01-01T00:00:00Z" GIT_COMMITTER_DATE="2026-01-01T00:00:00Z"
printf '# readme\n' > README.md
printf 'fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n' > a.rs
git add -A && git commit -qm c0
export GIT_AUTHOR_DATE="2026-01-02T00:00:00Z" GIT_COMMITTER_DATE="2026-01-02T00:00:00Z"
printf 'fn add(a: i32, b: i32) -> i32 {\n    // sum\n    a + b\n}\n' > a.rs
mkdir src && printf 'fn plus(x: i32, y: i32) -> i32 {\n    x + y\n}\n' > src/b.rs
git add -A && git commit -qm c1
export GIT_AUTHOR_DATE="2026-01-03T00:00:00Z" GIT_COMMITTER_DATE="2026-01-03T00:00:00Z"
git mv src/b.rs src/c.rs && git rm -q README.md && git commit -qm c2
git checkout -q -b side
export GIT_AUTHOR_DATE="2026-01-04T00:00:00Z" GIT_COMMITTER_DATE="2026-01-04T00:00:00Z"
printf 'fn d() {}\n' > d.rs && git add -A && git commit -qm s1
printf 'fn d() -> i32 { 1 }\n' > d.rs && git add -A && git commit -qm s2
git checkout -q main
export GIT_AUTHOR_DATE="2026-01-05T00:00:00Z" GIT_COMMITTER_DATE="2026-01-05T00:00:00Z"
git merge -q --no-ff side -m merge
