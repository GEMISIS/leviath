# leviath-legacy-runs

This crate is temporary. It will be removed before Leviath 1.0.

It converts a run directory written in the old layout into a single run file.
An old run was spread over an LVR1 journal (`run.lvr`), `meta.json`,
`context.json`, `stages.json`, `fanout.json`, `interactions.json`, a copy of
its blueprint and a `blobs/` directory. `convert` reads them all and writes
one LVR2 run file in their place:

- the run's spec, rebuilt from its metadata and the blueprint it ran;
- its code and stored parts, once each by digest;
- the state it started in, one delta per journal step that maps onto the new
  format, and the state it was last in.

The old files are moved into `legacy/` inside the run directory, not deleted.
A directory that already holds a run file is refused, so converting twice
does nothing.

An old run did not record everything a run file holds. Each value the
conversion had to fill in is listed in the returned report, with the value
used and the reason, and the same lines go into the run's log. The
environment fingerprint is always left empty: an old run never recorded what
it relied on from the machine, so a resume treats it as unknown and does not
compare it.

Alpha builds wrote run files in an earlier binary layout, layout 2, which
differs from this build's only in how the spec holds each stage's model and
reply cap. `upgrade` rewrites such a file in place: the spec is read in the
old shape and written in the new one, every other frame is copied byte for
byte, and the file as it was is kept as `legacy/run.v2.lvr`.
`needs_upgrade` tells one from its header alone.

It also holds the only reader left for the old `agent.leviath` blueprint
format. `migrate` turns one into an `agent.toml` describing the same run,
which is what `lev blueprint migrate` writes. Nothing else reads an
`agent.leviath` any more.

The CLI is the only crate that depends on this one. Nothing else should.
It is never published.
