# Changelog

## 2026.09.25

### Bug Fixes

- **land:** keep AI commit plans accurate and reviewable ([`9d37158`](https://github.com/binbandit/kite/commit/9d371589c056ac6e46a55aa9c69a1e7d17a9370d))

## 2026.09.17

### Bug Fixes

- **land:** accept formatting from successful commit hooks ([`1e94053`](https://github.com/binbandit/kite/commit/1e940530dd079748ba8c4f58d8b37d34eaeee940))

## 2026.09.15

### Features

- **cli:** implement undo to revert last land by rewinding to pre_land ([`fd03bb1`](https://github.com/binbandit/kite/commit/fd03bb1a37e1491ee5ef8c99293e5333a7e28d65))
- **core:** implement commit_git with detailed error rendering and helper utilities ([`1a0887b`](https://github.com/binbandit/kite/commit/1a0887b18bd1653aabf99c7f587e3c68dd762d96))
- **land:** add land module ([`fcaefe1`](https://github.com/binbandit/kite/commit/fcaefe129f122d55f07adaf0ca6bdf7756cc013b))
- **git:** add git helpers ([`3d43364`](https://github.com/binbandit/kite/commit/3d43364115f34b3033b1e34bceca79cde3a24681))
- **main:** add CLI entrypoint ([`77f6f4f`](https://github.com/binbandit/kite/commit/77f6f4f20c2ff1555fb89ef732d4fc62be59d29c))
- **synth:** add history synthesis logic ([`63bb27f`](https://github.com/binbandit/kite/commit/63bb27fa6679eb47cfeb60691ddca972e307ad4f))
- **git:** add git helpers ([`e46af89`](https://github.com/binbandit/kite/commit/e46af89f630e0d48538640198df951a532c04a23))
- **land:** add land module ([`54defd1`](https://github.com/binbandit/kite/commit/54defd1cf2e2e59502e129ad8e68d6cd9e9a70d0))
- **main:** add CLI entrypoint ([`2679d72`](https://github.com/binbandit/kite/commit/2679d72503c0021337158bc701aa300a3af2cd47))
- **git:** add is_inside_git_repository helper ([`e662ba4`](https://github.com/binbandit/kite/commit/e662ba420ea16c8cce545e72aa047296c7d4027c))
- **main:** extend CLI to render help outside git repo ([`1a739e3`](https://github.com/binbandit/kite/commit/1a739e33ddfbcc5be1258ffde06ef008b2d3edf0))
- **land:** allow land with temporary dirty worktree stash ([`77de00c`](https://github.com/binbandit/kite/commit/77de00c4fb4a12d412b55f2e00ecb40b80973e33))
- **synth:** add provider timeout config ([`72969f2`](https://github.com/binbandit/kite/commit/72969f2836b1a4c9470eb926d42bdcbd600ca7db))
- **synth:** add portkey api key header ([`013449f`](https://github.com/binbandit/kite/commit/013449fb054dd1e3b4def4bf7bb267286293e961))
- **pr:** prune template boilerplate, authoritative skills, and refresh open PRs (#2) ([`e29c504`](https://github.com/binbandit/kite/commit/e29c504cb11026ce00fafec21391119f987307d3))
- **ai:** switch synthesis and pr drafting to OpenAI Responses API ([`fa838be`](https://github.com/binbandit/kite/commit/fa838bed64cf116c77f8549a6a09aa167784dbe8))
- **land:** add hunk-level planning and exact-tree verification ([`e57c473`](https://github.com/binbandit/kite/commit/e57c473904a6bf9e4bf1119350067d3de1dd8310))
- **land:** stage hunk plans and fall back to whole files ([`fc78f7a`](https://github.com/binbandit/kite/commit/fc78f7a20b6581063a949ed1823779d12d37abb2))
- **land:** tighten patch context and add split-edit land test ([`f3c4bca`](https://github.com/binbandit/kite/commit/f3c4bcaf2fd4478cc4a338e0d14406e194771eb3))
- **hunks:** cap prompt bodies with fair per-unit budgeting ([`fedc141`](https://github.com/binbandit/kite/commit/fedc141b35ebaaf066dc1b1a297b890b66f32dbb))
- **land:** show split hunk headings in the plan ([`ab73fe2`](https://github.com/binbandit/kite/commit/ab73fe2fcf1e871fb5d2fe02cb6cae18da93afd5))
- **undo:** reverse the last quicksave, not just the last land ([`55f9576`](https://github.com/binbandit/kite/commit/55f95767bfbce8ade3b8b79076ca8d6e1d599165))
- support AI_GATEWAY_API_KEY env var from switchboard ([`e442a96`](https://github.com/binbandit/kite/commit/e442a96d734eb5c743817b22f19be188d47be706))
- **git:** support detached HEAD worktrees ([`f0472e5`](https://github.com/binbandit/kite/commit/f0472e51f011b4087935ad1702ed119c3fc9d629))
- **land:** append tag to landed commit titles ([`6f205e5`](https://github.com/binbandit/kite/commit/6f205e5ca6f4f688219db4115373b36ebad481bd))
- **land:** add hook control and tag support ([`69e5542`](https://github.com/binbandit/kite/commit/69e5542505b80c408f4dd5d1a34c2f1ec83cd21e))
- **land:** wire no-verify through cli and save ([`c662da1`](https://github.com/binbandit/kite/commit/c662da1777a5653961a215baf7023fb94c91594e))
- **land:** group saves by file ([`a2319dd`](https://github.com/binbandit/kite/commit/a2319dde5fbf496bff556c999e35f3788a06a427))
- **land:** fetch saved paths and diff together ([`3efc882`](https://github.com/binbandit/kite/commit/3efc882fd3e33acac8fc54bb6dcfda22483d949f))
- **land:** land file-level saves with shared overflow notes ([`d0f394a`](https://github.com/binbandit/kite/commit/d0f394ae5f9f158271365aa3cc0522bc46ae0158))
- **cli:** add push alias for publish ([`e263112`](https://github.com/binbandit/kite/commit/e263112b2526ddcdacdde4254fb7f999d6f04d9d))
- **cli:** add short flag for land push ([`d5708af`](https://github.com/binbandit/kite/commit/d5708af7b8f78bf31326eb6cc629333b649f17cd))
- **land:** ignore bare pre-land pointers ([`ee77efb`](https://github.com/binbandit/kite/commit/ee77efbddef52d02340650f86346fbe24cf55005))
- **cli:** let undo bypass recovery blocking ([`370b968`](https://github.com/binbandit/kite/commit/370b968943f891b0d020b77c88b2320f028cfbc5))

### Bug Fixes

- **land:** suppress local/openai error details and fall back to manual when both providers fail ([`9d9d92b`](https://github.com/binbandit/kite/commit/9d9d92b7d97e67706d4074047872299a148ebe13))
- **land:** make landing safer and repo-aware ([`c7124a5`](https://github.com/binbandit/kite/commit/c7124a565e6e3d168e1655381bfeabf7b9ec06ac))
- **land:** map stash pop output to unit result ([`c72bb9a`](https://github.com/binbandit/kite/commit/c72bb9a4e91e25e34a8eb0feca1c2fa17be84295))
- switch to existing kt go branches ([`3e01da2`](https://github.com/binbandit/kite/commit/3e01da22b2e311daec0152edc8d255f2326b92e0))
- cache repo root lookup ([`3ae309f`](https://github.com/binbandit/kite/commit/3ae309f23269c0493ca53bfceecc4f98e0840dd3))
- **land:** simplify stash restore error handling ([`bae55c1`](https://github.com/binbandit/kite/commit/bae55c1abbd49d21a1674f5a24cd371728ae3d1e))
- **synth:** simplify json parsing branches ([`dfd597d`](https://github.com/binbandit/kite/commit/dfd597da5eab73564f4c898eea15b257cac7fb5e))
- **land:** simplify land summary tree rendering ([`3294bf9`](https://github.com/binbandit/kite/commit/3294bf9988262768053e7de99a53a4b44cc25ab2))
- **synth:** dedupe validation ids inline and require dependency-ordered groups ([`ebc2ec7`](https://github.com/binbandit/kite/commit/ebc2ec7f31b82dfb9df9733b7e82480a537b7b4d))
- **synth:** join leftovers to their file groups ([`003c8d1`](https://github.com/binbandit/kite/commit/003c8d1f599fbc5dd470d96e09b632c15afcc44b))
- **land:** drop raw @@ headers from the plan ([`0e046e0`](https://github.com/binbandit/kite/commit/0e046e0da2754b633959313ab4d3681fe9cb4ab8))
- stop kt undo and kt publish from destroying other branches ([`28c3aef`](https://github.com/binbandit/kite/commit/28c3aef941e175690efc658f3882c3676d4553f3))
- keep CRLF diffs intact and never land a save-shaped message ([`e2e40dd`](https://github.com/binbandit/kite/commit/e2e40dd7b6cc6dbb8755ae46ef456a087d92a7e1))
- make AI failures diagnosable and spinner cleanup unconditional ([`45abae1`](https://github.com/binbandit/kite/commit/45abae126454352dd5103f54fae508b9c68b41a4))
- **land:** put the user back on their branch when landing fails ([`607bde3`](https://github.com/binbandit/kite/commit/607bde322382db31d3ae737844d43ab4c78cb7f3))
- **land:** stop creating a branch for landing at all ([`6497a05`](https://github.com/binbandit/kite/commit/6497a05b193119dbd60243bb97f484e78ce2bb6d))
- support anthropic models via json_object fallback ([`f08a5c9`](https://github.com/binbandit/kite/commit/f08a5c995831f82dbb4e5d66c2d685993b8b5d2b))
- **pr:** never publish unfilled templates ([`89ffd6f`](https://github.com/binbandit/kite/commit/89ffd6fa7b8e19bfad6a4e72767e73adcff3c11a))
- **cli:** preserve work across landing and publishing ([`1c3a59f`](https://github.com/binbandit/kite/commit/1c3a59f299f224ebab4b5e47914a9d9e86539e55))

### Performance

- trim dependencies and tune the release profile (#1) ([`d937e4f`](https://github.com/binbandit/kite/commit/d937e4fd5d9271cc2f9803527f0c360d7cc13008))
- undo the flow costs this branch introduced ([`497a588`](https://github.com/binbandit/kite/commit/497a58884dea6aca34097cc3e32c1c600074746e))

### Refactors

- **git:** simplify git API, add has_remote/get_kite_base, guard fetch/push by remote, and implement kite-base unwind and improved staging/commit in land ([`797a324`](https://github.com/binbandit/kite/commit/797a3247f4b2db7decaf4d55ae051731bb3b9b3f))
- remove the detached-HEAD-unsafe branch lookup ([`fe6874f`](https://github.com/binbandit/kite/commit/fe6874f873af0a26a90ee7df51bce620cee36895))

### Documentation

- **readme:** add project title to documentation ([`4219a42`](https://github.com/binbandit/kite/commit/4219a424150b714663a6fe066335155050f780d3))
- **readme:** reflect branding and usage updates ([`1591943`](https://github.com/binbandit/kite/commit/1591943e173c56a6c2454107fc89e73836ebfb7f))
- **readme:** document quicksaves skip git hooks and landing commits trigger hooks ([`567784f`](https://github.com/binbandit/kite/commit/567784fa7c1930081305b6ff8b96b98c4a728cac))
- refresh kite usage and ai provider docs ([`8762330`](https://github.com/binbandit/kite/commit/8762330c5caed9c7a0bdc35f2d6d2e8f17569d2b))
- **use-kite:** update docs ([`a6fc0e7`](https://github.com/binbandit/kite/commit/a6fc0e7cd489c79e9c3e5112c92f61c06ca1bad3))
- document provider timeout env vars ([`0d87931`](https://github.com/binbandit/kite/commit/0d87931e88a5c48ce3ce698abece4725ba224750))
- clarify leftover hunk handling ([`0e0fa7b`](https://github.com/binbandit/kite/commit/0e0fa7b3bf61f0c34541ee8864cc6b99eb0fd3ca))
- describe the corrected undo, publish, and go behaviour ([`22f3f8e`](https://github.com/binbandit/kite/commit/22f3f8e1d9052b3066bcd496e51be05ca994d652))
- explain recovery and contribution workflows ([`274d7db`](https://github.com/binbandit/kite/commit/274d7dbcc964db5ea1fc8c269759d64e32c9a1e1))

### CI

- validate changes and publish tested release artifacts ([`720e06d`](https://github.com/binbandit/kite/commit/720e06d424b8567094d0203e269d3d3cf70c1654))

### Tests

- **src/main.rs:** add unit tests for JSON parsing helpers ([`00f5f79`](https://github.com/binbandit/kite/commit/00f5f79d787e6e8a7076bb7d9febbfd43f42549d))
- **test-support:** add test helpers ([`db0187b`](https://github.com/binbandit/kite/commit/db0187b926f48cf12dcde144b7e9babaf517c061))

### Chores

- **gitignore:** add .gitignore ([`d1cc4dc`](https://github.com/binbandit/kite/commit/d1cc4dc290485fae736391d8dafdb36be92dc854))
- **lockfile:** add Cargo.lock ([`9d787a3`](https://github.com/binbandit/kite/commit/9d787a34b96b29b24dda9757bb042e6fb71dc4b5))
- unclassified updates ([`efe6cd3`](https://github.com/binbandit/kite/commit/efe6cd3ec009b774244912846d60b16b7e857e94))
- **ci:** add daily-release workflow ([`8b8ae30`](https://github.com/binbandit/kite/commit/8b8ae3047982a77f6b3c7c8d0d43a26744dedeac))
- **lockfile:** update Cargo.lock ([`2f4f5fb`](https://github.com/binbandit/kite/commit/2f4f5fb5dfaf60a1f368465a13aea2c4cf281ab2))
- **lockfile:** update Cargo.lock ([`9a25750`](https://github.com/binbandit/kite/commit/9a2575001c2eddf8208dd62a815769cd5f29f380))
- **deps:** update hashbrown ([`9b8f816`](https://github.com/binbandit/kite/commit/9b8f816183230417280b6d8801586cfddec535ee))
- add justfile with install, build, and check recipes ([`69f7536`](https://github.com/binbandit/kite/commit/69f7536516597c05099175fc18eaadbe5c878c00))
- format ai response requests ([`7ec707d`](https://github.com/binbandit/kite/commit/7ec707dcd67c0afe2fcfe0570ef3eec7f42356c3))
- format ai test string ([`ee4784a`](https://github.com/binbandit/kite/commit/ee4784a4ab9491d0303f29d31b41f9f325ca67b7))
- document land hook and tag behavior ([`d6c2cc5`](https://github.com/binbandit/kite/commit/d6c2cc51add71cc216ccd976c75af9ec8dfa76c9))
- document file-level land behavior ([`d69f32c`](https://github.com/binbandit/kite/commit/d69f32cc5c6f4648f1b010cb96694ab15213f1cc))
- add overflow note helper ([`26c5727`](https://github.com/binbandit/kite/commit/26c5727009c4ae97e62fe5598940dc1e3fc0d9e1))
- optimize changed file rendering ([`019e71d`](https://github.com/binbandit/kite/commit/019e71d59b2a62f6efc791fd9b3bf76fad7acac4))
- tighten commit group validation ([`69fd4fb`](https://github.com/binbandit/kite/commit/69fd4fb15b330258fcc373d2d74ef1b8c52178d1))
- document publish alias ([`d4fdc14`](https://github.com/binbandit/kite/commit/d4fdc14ea5a7f9e7b23b2334f11c14f5931aca65))
- document land push short flag ([`16a7832`](https://github.com/binbandit/kite/commit/16a78322ae5ae37a45345a6250d4e6f5101808bb))

### Other Changes

- Refactor land output to use shared tree render helpers ([`af4a47c`](https://github.com/binbandit/kite/commit/af4a47c1d14688af03032e251124064ebc758e30))
- Add use-kite skill and installation docs ([`2c52a72`](https://github.com/binbandit/kite/commit/2c52a72a13854f98f790bdcc6aa279b4d227494c))
- Respect pre-staged changes when creating `kt` quicksaves ([`71978fc`](https://github.com/binbandit/kite/commit/71978fc9c2e650417a0ab9e387adc7e27305be18))
- Update README clone command with GitHub URL ([`37dfb2d`](https://github.com/binbandit/kite/commit/37dfb2d05e036e464859ce0c64e745e07901a69f))
- Document OpenAI model env vars and default fallback model ([`2d932c2`](https://github.com/binbandit/kite/commit/2d932c2b5453d7c1a42e794c9f963fb68856d827))
- Refactor daily release workflow into staged prepare/build/publish jobs ([`8067541`](https://github.com/binbandit/kite/commit/80675414d1f238def959edf007eafcd0530e5227))
- Pull remote changes before pushing during land ([`35f4d16`](https://github.com/binbandit/kite/commit/35f4d16ff5f73edd913cc6652898df979330c4d6))
