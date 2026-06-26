# A Small Build Tool

I want a small build tool that I can call from a feature branch that does the
following:

1. Grabs all of the commits on this feature branch not present on origin/master
2. Identifies what crates have changed from those commits
3. Allows you to specify whether you'll do a patch, minor, or major bump
4. Updates the Cargo.toml for that crate to the new version
5. Generates the new lockfile
6. Commits these changes to the feature branch
7. Pushes to the remote
8. Opens a PR from the remote branch to origin master
9. (maybe) have a little gha tool that checks if the version changed from the
   last one, and if so, create a tag for it so the images get built.

This works because origin/master always contains the working, up to date version
of the code. No force pushes, PR branches always have to be up to date, and
status checks need to pass. This means I don't need to worry about using tags or
the release SHA to find what the current version of the package is, I just take
whatever is in origin/master.

This does mean I'll get a decent amount of conflicts if I have many feature
branches open, but I think that's an acceptable trade off, we always take the
remote changelog over our own, since it will get regenerated.

# Status

I have just cowboyed 1-6, and I am hoping to have 7-9 done within a few days
time permitting. Once it seems like its working, I will actually write it
correctly and provide a nice TUI interface for it.

If you happen to stumble on this for some reason or another, I think
[release-plz](https://github.com/release-plz/release-plz) is probably what you
are looking for.
