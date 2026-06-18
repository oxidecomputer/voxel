# omdb fails with `libpq.so.5: open failed` after upgrading to omicron release 20 (stale `pq-sys` build cache)

## Summary

After bumping the omicron checkout used by the testbed up to **release 20** - which
includes omicron [#9869](https://github.com/oxidecomputer/omicron/pull/9869), "Use
PostgreSQL v18 client libraries" - `omdb` (and any other diesel/`pq-sys`-linked
binary) can be built with a stale `RUNPATH` pointing at the *old* PostgreSQL client
library directory. The control-plane zones meanwhile ship the *new* one, so the
runtime linker can't find `libpq.so.5`.

This only happens when you **reuse an omicron build tree / build host across the
PostgreSQL 13 -> 18 transition** - i.e. exactly the situation when upgrading an
existing testbed setup from a prior release. A fresh checkout / build host is
unaffected, which is why CI does not catch it.

## Symptom

In the switch zone (`oxz_switch`, on g0 and/or g3):

```
root@oxz_switch:~# omdb
ld.so.1: omdb: fatal: libpq.so.5: open failed: No such file or directory
Killed
```

`ldd` confirms every other dependency resolves - only `libpq` is missing:

```
root@oxz_switch:~# ldd /opt/oxide/omdb/bin/omdb
        ...
        libpq.so.5 =>    (file not found)
        ...
```

## Who is affected

Anyone who:

- has an existing testbed build host that previously built a **pre-release-20**
  omicron (PostgreSQL **13** client libraries), **and**
- upgrades the omicron checkout to **release 20 or later** (PostgreSQL **18**,
  omicron #9869), **and**
- rebuilds with `./config/build-packages.sh` **without** clearing the Cargo cache.

## Root cause

omicron links `libpq` via `diesel` -> `pq-sys`. Two independent things must agree on
the PostgreSQL major version:

1. **The binary's `RUNPATH`.** omicron's `rpaths` crate bakes the libpq directory
   that `pq-sys` discovered *at build time* into every diesel-linked binary's
   `RUNPATH`. `pq-sys` discovers it via `pg_config`, which follows the
   `pkg set-mediator postgresql` setting.
2. **What the zone ships.** `package-manifest.toml` **hardcodes** the libpq
   directory it copies into zones - `omicron-nexus` and `switch_zone_setup` both
   copy `/opt/ooce/pgsql-18/lib/amd64`.

For omdb to run, (1) must equal (2).

The trap: **`pq-sys`'s build script only re-runs when `PQ_LIB_DIR`,
`PQ_LIB_STATIC`, `TARGET`, or `PG_CONFIG_*` change.** It does **not** re-run when
you `pkg install library/postgresql-18` and `pkg set-mediator -V 18 postgresql`.
So an incremental build after the upgrade reuses the cached **pgsql-13** discovery
and bakes `-R/opt/ooce/pgsql-13/lib/amd64` into the binaries, while
`omicron-package` faithfully ships **pgsql-18** into the zones. Mismatch ->
`open failed`.

Concretely, the broken `omdb`:

```
RUNPATH  ...:/opt/ooce/pgsql-13/lib/amd64:...     # binary looks here
```

vs. the switch zone, which only has:

```
/opt/ooce/pgsql-18/lib/amd64/libpq.so.5           # ...but the lib is here
```

This is latent in *all* diesel-linked binaries built in that pass (nexus,
sled-agent, ...); `omdb` is just the one operators hit interactively in the switch
zone.

## Immediate workaround (running rack, no rebuild)

In the affected switch zone(s) - `oxz_switch` on **both g0 and g3** - symlink the
shipped libpq into the path the binary expects:

```bash
mkdir -p /opt/ooce/pgsql-13/lib/amd64
ln -s /opt/ooce/pgsql-18/lib/amd64/libpq.so.5 /opt/ooce/pgsql-13/lib/amd64/libpq.so.5
```

(Adjust the version numbers to match the mismatch you observe.) This is ephemeral -
lost on `a4x2 destroy`/relaunch or any zone reinstall.

## Proper fix

Rebuild with the `pq-sys` cache busted so it re-discovers libpq:

```bash
cd $OMICRON
cargo clean -p pq-sys

cd testbed/a4x2
export PQ_LIB_DIR=/opt/ooce/pgsql-18/lib/amd64   # pin it; pq-sys tracks this var
./config/build-packages.sh
```

Verify *before* relaunching - the new `omdb`'s `RUNPATH` must point at the same
pgsql version the zone ships:

```bash
cd cargo-bay/g0/omicron/out
gzip -dc switch-softnpu.tar.gz | tar xf - root/opt/oxide/omdb/bin/omdb
elfdump -d root/opt/oxide/omdb/bin/omdb | grep RUNPATH
# expect: .../opt/ooce/pgsql-18/lib/amd64/...
```

Then `pfexec ./a4x2 destroy && pfexec ./a4x2 launch`.

## Proposed permanent fix in the testbed

`pq-sys` cannot tell that the postgres mediator changed, so `build-packages.sh`
should not rely on Cargo's incremental cache being correct across that transition.
Options, roughly in order of preference:

1. **Have `build-packages.sh` run `cargo clean -p pq-sys` before building.**
   `pq-sys` is a tiny crate; cleaning + recompiling it every run costs effectively
   nothing and guarantees libpq is re-discovered.
2. **Have `build-packages.sh` export `PQ_LIB_DIR`** (e.g.
   `export PQ_LIB_DIR=$(pg_config --libdir)`), which both pins the path and is a
   tracked rebuild trigger for `pq-sys`.
3. At minimum, **document this in the testbed upgrade notes for release 20.**
