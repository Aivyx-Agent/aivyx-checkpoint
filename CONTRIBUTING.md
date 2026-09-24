# Contributing to aivyx-checkpoint

Thanks for your interest in contributing.

## The Contributor License Agreement (required)

Aivyx Checkpoint is **source-available under [BUSL-1.1](LICENSE)**
and is offered under a dual model — **free for personal/non-commercial
use, paid for commercial use** (see [`COMMERCIAL.md`](COMMERCIAL.md)).
For that model to be lawful, the project must hold the right to license
**all** of the code — including your contributions — under both the
BUSL-1.1 terms and separate commercial terms.

A plain "inbound = outbound" contribution does **not** give the project
the right to commercially sublicense your code. So, **before any
contribution can be merged, you must agree to the [Contributor License
Agreement (`CLA.md`)](CLA.md).** In short, the CLA: you keep
your copyright, and you grant the Licensor a broad, irrevocable license
to use, relicense, and **commercially sublicense** your contributions.
This agreement covers contributions to any Aivyx-Agent repository, not
just this one — read [`CLA.md`](CLA.md) for the exact terms,
it is short.

**How you agree:** every commit in your pull request must carry a
`Signed-off-by` trailer matching the author, added automatically by
committing with `-s`:

```sh
git commit -s -m "your message"
```

By signing off you certify the [Developer Certificate of
Origin](#developer-certificate-of-origin) **and** accept the
[CLA](CLA.md) for that contribution. A maintainer cannot merge a
PR whose commits are not signed off. If you are contributing on behalf
of an employer, make sure you have their permission first (the CLA
covers this).

> **Why this exists:** without it, the relicense and the commercial
> offering could not legally cover contributed lines. This gate must
> precede any external PR — see the ecosystem's own
> [`LICENSING.md`](https://github.com/Aivyx-Agent/aivyx-ecosystem/blob/main/LICENSING.md).

## Before you open a PR

- For anything beyond a small fix, please open an issue first to describe
  the shape of the change so we can agree on the approach.
- Keep changes focused — a PR should do one thing.

## Building & testing

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo fmt
```

## Developer Certificate of Origin

By making a contribution to this project, you certify the
[Developer Certificate of Origin 1.1](https://developercertificate.org/):

> 1. The contribution was created in whole or in part by you and you have the
>    right to submit it under the open source license indicated in the file; or
> 2. The contribution is based upon previous work that, to the best of your
>    knowledge, is covered under an appropriate open source license and you have
>    the right under that license to submit that work with modifications,
>    whether created in whole or in part by you, under the same license (unless
>    you are permitted to submit under a different license), as indicated in the
>    file; or
> 3. The contribution was provided directly to you by some other person who
>    certified (1), (2) or (3) and you have not modified it.
> 4. You understand and agree that this project and the contribution are public
>    and that a record of the contribution (including all personal information
>    you submit with it, including your sign-off) is maintained indefinitely and
>    may be redistributed consistent with this project and the requirements
>    stated above.

Your `Signed-off-by` line certifies the DCO above **and** accepts the
[CLA](CLA.md) for the signed contribution.
