## [1.13.1](https://github.com/louislafosse/hacksor/compare/v1.13.0...v1.13.1) (2026-10-28)


### Bug Fixes

* **engagement:** find the engagement when the chat opens above it ([2608ee2](https://github.com/louislafosse/hacksor/commit/2608ee2d04420212cc853422445e26d348792d92))

# [1.13.0](https://github.com/louislafosse/hacksor/compare/v1.12.0...v1.13.0) (2026-10-28)


### Bug Fixes

* **chat:** stop responds immediately instead of waiting out the turn ([c5ad6ca](https://github.com/louislafosse/hacksor/commit/c5ad6ca0a55a3126c2212bca30f09693b0a2e3fa))
* **engagement:** re-attach a chat to its engagement directory ([cf2c0ad](https://github.com/louislafosse/hacksor/commit/cf2c0aded60948fc09bbd110b40fabf42bfbf548))


### Features

* **categories:** pick any emoji, by name ([7d59115](https://github.com/louislafosse/hacksor/commit/7d591153ee9ad472b751ea832bb2fa73a316095e))
* **engagement:** a view of the findings, report and evidence in the app ([a4c5364](https://github.com/louislafosse/hacksor/commit/a4c5364eb21c73a4604a7f92c8e023c01d9f27d5))

# [1.12.0](https://github.com/louislafosse/hacksor/compare/v1.11.0...v1.12.0) (2026-10-28)


### Features

* **agent:** engagement directories, throwaway inboxes, prompt corrections ([31ffedc](https://github.com/louislafosse/hacksor/commit/31ffedcc691d010a6679a2c8bfd53977c1dfd3b9))
* **chat:** show context use, group turns, and make long output readable ([c70fe8b](https://github.com/louislafosse/hacksor/commit/c70fe8bf25a8f824c30f7cdab07a8017611f2552))
* **security:** warn before an agent turn leaves over your own connection ([986573f](https://github.com/louislafosse/hacksor/commit/986573ff776cea6cb95878ec9dc72fe1b4908ae4))
* **ui:** move the frontend onto React, Tailwind and shadcn ([9de4377](https://github.com/louislafosse/hacksor/commit/9de4377151a7da36404d88ac0ad9d4cf681fe0e7))
* **updater:** install updates in place instead of opening the release page ([be7fc20](https://github.com/louislafosse/hacksor/commit/be7fc20e9ca9ea077ca8c50af2e253861c5c7161))

# [1.11.0](https://github.com/louislafosse/hacksor/compare/v1.10.0...v1.11.0) (2026-10-19)


### Features

* **chats:** name a chat from its first message, and keep renames ([f7a6c92](https://github.com/louislafosse/hacksor/commit/f7a6c926edefdc62954a09bd995e26c0111ebcc9))

# [1.10.0](https://github.com/louislafosse/hacksor/compare/v1.9.1...v1.10.0) (2026-10-19)


### Features

* **providers:** add abliteration.ai as a composer provider ([e90da12](https://github.com/louislafosse/hacksor/commit/e90da12a6cad471eb37b5ff0e6cabaf9d63ad43b))
* **runtime:** notice when the published image is newer, and fetch it ([7ded478](https://github.com/louislafosse/hacksor/commit/7ded4788ac15a70f3986b43e7b1c6376877f7c1f))

## [1.9.1](https://github.com/louislafosse/hacksor/compare/v1.9.0...v1.9.1) (2026-10-19)


### Bug Fixes

* **release:** ad-hoc sign the macOS bundles so they stop reporting "damaged" ([b105a03](https://github.com/louislafosse/hacksor/commit/b105a03a96d216a3f55a82a61b22f2a7a065c4fc))

# [1.9.0](https://github.com/louislafosse/hacksor/compare/v1.8.0...v1.9.0) (2026-10-19)


### Features

* **prompts:** pick the right HTTP client, and reach for an enum tool first ([7e32e9f](https://github.com/louislafosse/hacksor/commit/7e32e9f1923b89495eb961b96b5520134d0f9511))
* **runtime:** add curl_cffi for browser-matching TLS fingerprints ([50d56c3](https://github.com/louislafosse/hacksor/commit/50d56c3401831a8de9428240e960914a925ee7f5))

# [1.8.0](https://github.com/louislafosse/hacksor/compare/v1.7.0...v1.8.0) (2026-10-19)


### Features

* **updates:** show the version, check for a newer one, and what changed ([a7e7102](https://github.com/louislafosse/hacksor/commit/a7e7102aa66dac708fe23250f8207154af498523))

# [1.7.0](https://github.com/louislafosse/hacksor/compare/v1.6.0...v1.7.0) (2026-10-19)


### Bug Fixes

* **browser:** build camofox's native deps, and document the tabId they need ([700d473](https://github.com/louislafosse/hacksor/commit/700d4739c27fbc8f57eb88bd51f6d585648ad3b6))
* **proxy:** say when a stale scope is dropping every capture ([900002c](https://github.com/louislafosse/hacksor/commit/900002c1db52f09b3d91ec39404b4ea047127ded))


### Features

* **prompts:** insist that submitted data is invented, never the operator's ([0472f48](https://github.com/louislafosse/hacksor/commit/0472f48f8be0803e883f9bad3f400ce00111f4ef))

# [1.6.0](https://github.com/louislafosse/hacksor/compare/v1.5.3...v1.6.0) (2026-10-18)


### Bug Fixes

* **compaction:** count the auto-compact threshold since the last compaction ([36cd2eb](https://github.com/louislafosse/hacksor/commit/36cd2eba31dea4fac56275b122e86e6040f42a75))
* **opencodex:** repair the "run ocx login" dead end, and stop a stray kill ([a86b8bd](https://github.com/louislafosse/hacksor/commit/a86b8bd66a54945f94504c1411c6f58056b09758))
* **ui:** highlight restored messages up front instead of while scrolling ([d043172](https://github.com/louislafosse/hacksor/commit/d043172ec03b909459920e4ef681ed60cc870ce3))


### Features

* **branch:** branch a chat at one message, with its context extracted ([124733b](https://github.com/louislafosse/hacksor/commit/124733bb39ff49619b60c99ef6e426635263f788))

## [1.5.3](https://github.com/louislafosse/hacksor/compare/v1.5.2...v1.5.3) (2026-10-13)


### Bug Fixes

* **models:** list ocx's direct upstreams in the OpenCodex picker ([5b72446](https://github.com/louislafosse/hacksor/commit/5b72446e09c10c6c62e37ec6c1f30172c1e24220))

## [1.5.2](https://github.com/louislafosse/hacksor/compare/v1.5.1...v1.5.2) (2026-10-13)


### Bug Fixes

* **providers:** route Cheaper Inference through ocx's openai-chat adapter ([f959546](https://github.com/louislafosse/hacksor/commit/f9595467b9bfed2356cfe8cf2a9183dfbd3631d1))
* **ui:** resume following a live answer when you scroll back to the bottom ([4282b0d](https://github.com/louislafosse/hacksor/commit/4282b0d68345ccbbf7f46cfc8e517207bcaedfa1))

## [1.5.1](https://github.com/louislafosse/hacksor/compare/v1.5.0...v1.5.1) (2026-10-13)


### Bug Fixes

* **models:** tolerate ocx advisories in parsed command output ([25ddd27](https://github.com/louislafosse/hacksor/commit/25ddd27d87efa8e2a6b79c44a770cb396db61662))
* **ui:** keep scrolling up from snapping back in a restored chat ([6298f20](https://github.com/louislafosse/hacksor/commit/6298f202adaae5604d98344fd2b05a536eff4c04))

# [1.5.0](https://github.com/louislafosse/hacksor/compare/v1.4.0...v1.5.0) (2026-10-13)


### Bug Fixes

* **platform:** find docker and the other CLIs when launched from a GUI ([3251a99](https://github.com/louislafosse/hacksor/commit/3251a99e432710caeb9903925e749451a09c8ff4))
* **runtime:** patch ocx's discovery timeout by content, not by filename ([4556318](https://github.com/louislafosse/hacksor/commit/4556318684bd9474b62974603678d7dc3836f6ed))
* **ui:** don't replace message DOM while the user is selecting in it ([f728e3b](https://github.com/louislafosse/hacksor/commit/f728e3b8831cb994cb7c3eb970723b1f556b27e7))


### Features

* **runtime:** add REA, Ghidra and Unicorn to the runtime image ([dfa73b6](https://github.com/louislafosse/hacksor/commit/dfa73b691fabe9080bedfd9478725a499d8b9d52))

# [1.4.0](https://github.com/louislafosse/hacksor/compare/v1.3.3...v1.4.0) (2026-10-04)


### Bug Fixes

* **ui:** re-arm auto-follow when the view is already at the bottom ([9360f13](https://github.com/louislafosse/hacksor/commit/9360f13262d48b80245e12f17d96f4eb025b7cae))


### Features

* **ui:** flag a model that writes its tool call as text ([71d3255](https://github.com/louislafosse/hacksor/commit/71d32556ceec0effaa437acbdce5f32f20f0b38e))

## [1.3.3](https://github.com/louislafosse/hacksor/compare/v1.3.2...v1.3.3) (2026-09-29)


### Bug Fixes

* **sidebar:** don't block Recents behind the model catalog ([4d209f3](https://github.com/louislafosse/hacksor/commit/4d209f30f04ff6a12a1c601c9d9f19b17764c157))


### Performance Improvements

* **transcript:** ship only the slice of a chat the UI renders ([76033ae](https://github.com/louislafosse/hacksor/commit/76033ae3b61ca58214dfdb1c3b6f82aa436380a6))
* **ui:** highlight restored messages only as they come into view ([3ab5f02](https://github.com/louislafosse/hacksor/commit/3ab5f02f03a2dedf10fc00bd5fd13814cc6299fd))

## [1.3.2](https://github.com/louislafosse/hacksor/compare/v1.3.1...v1.3.2) (2026-09-28)


### Bug Fixes

* **runtime:** create the proxy's private codex home before starting it ([3971b4f](https://github.com/louislafosse/hacksor/commit/3971b4f2098dad86a052e2aa30a3bd18913fd5ef))

## [1.3.1](https://github.com/louislafosse/hacksor/compare/v1.3.0...v1.3.1) (2026-09-28)


### Bug Fixes

* **runtime:** stop the bundled proxy rewriting the user's own codex config ([7a2659a](https://github.com/louislafosse/hacksor/commit/7a2659aabdef691efd9986496231f83923944559))

# [1.3.0](https://github.com/louislafosse/hacksor/compare/v1.2.0...v1.3.0) (2026-09-27)


### Features

* **auto:** decide the recon-exploit handover with a judge, not keywords ([4664ff6](https://github.com/louislafosse/hacksor/commit/4664ff6f9895c4731acda2502c9d85c8188a36f5))
* **ui:** show a heartbeat when a running turn goes quiet ([b564707](https://github.com/louislafosse/hacksor/commit/b5647070c4ddb074da7c4a8221505a3353651033))


### Performance Improvements

* **recents:** keep chat-history disk work off the main thread ([76f8760](https://github.com/louislafosse/hacksor/commit/76f8760851408577372c4f833aae71e734356135))
* **ui:** stop re-rendering the whole answer on every streaming frame ([e5b7b7c](https://github.com/louislafosse/hacksor/commit/e5b7b7ce3de51715f2326112e5505063d02ef3ac))

# [1.2.0](https://github.com/louislafosse/hacksor/compare/v1.1.1...v1.2.0) (2026-09-27)


### Bug Fixes

* **models:** stop reporting "no models" while upstream discovery is in flight ([c4a13f0](https://github.com/louislafosse/hacksor/commit/c4a13f07ffc3554b9ec7f38eeb5f9627f50d748e))


### Features

* per-model operating specs and a Cheaper Inference provider ([5ae0dc1](https://github.com/louislafosse/hacksor/commit/5ae0dc109ca164bd67f9e90bf58cb0132b348df9))

## [1.1.1](https://github.com/louislafosse/hacksor/compare/v1.1.0...v1.1.1) (2026-09-15)


### Bug Fixes

* **ui:** clear the "runtime not ready" model error once the runtime is up ([6caf260](https://github.com/louislafosse/hacksor/commit/6caf260ba61ffb90d6168c2151a168f73598166b))

# [1.1.0](https://github.com/louislafosse/hacksor/compare/v1.0.0...v1.1.0) (2026-09-14)


### Features

* **runtime:** download the prebuilt image from GHCR on first run ([5e28e65](https://github.com/louislafosse/hacksor/commit/5e28e657bd922e7d1711bcfa259fbda06af195d2))

# 1.0.0 (2026-09-14)


### Bug Fixes

* **ci:** drop conflicting libappindicator3-dev; build macOS/Windows unsigned ([7c65f6c](https://github.com/louislafosse/hacksor/commit/7c65f6c478a8e1c7c54c88d54ee232b600fd8c83))
* **ci:** grant issues/PR write so semantic-release's success hook stops failing the job ([942007c](https://github.com/louislafosse/hacksor/commit/942007cb774c3c7c63e6cb9262e9afec8d2cc108))
* **harness:** capture app-server stderr for crash diagnostics ([45fe5bc](https://github.com/louislafosse/hacksor/commit/45fe5bc5c53630b5208056385e0e4084e17170c2))
* **harness:** enable the update_plan tool ([6efab4e](https://github.com/louislafosse/hacksor/commit/6efab4ea6ef2eff399a3a8f847706e7dc0b53ac3))
* **providers:** explain why a provider model list is empty ([fc9d360](https://github.com/louislafosse/hacksor/commit/fc9d360fe21c17fb9ab1813ce5a2c4d7556fa092))
* **providers:** list models from ocx live catalog; open web dashboard ([ff5b90c](https://github.com/louislafosse/hacksor/commit/ff5b90c0cb8715a10da2a8391b1ed4d9b672c9be))
* **providers:** sync + retry and sharper reason when a model list is empty ([d745da7](https://github.com/louislafosse/hacksor/commit/d745da7ef6135d4c19b6a8fd7bbeee42903f60c0))
* **proxy:** detach mitmdump so it survives the starting shell ([697ad45](https://github.com/louislafosse/hacksor/commit/697ad45d1e504ad18f866f97e5a8f430a9f3af5e))
* **runtime:** bridge ocx to the published port on Docker Desktop ([024d164](https://github.com/louislafosse/hacksor/commit/024d1643e5038e72ba2f275a431d1bc4c40db9cb))
* **runtime:** create/exec/remove containers via the docker CLI ([ec90f6b](https://github.com/louislafosse/hacksor/commit/ec90f6bde961ef77d34cdca13419904801c596ea))
* **runtime:** fix Windows Docker bind mount and improve status detection ([6034666](https://github.com/louislafosse/hacksor/commit/60346666a69e0ccc70ab846c1d206b6da7d4e73e))
* **subagent:** retry transient config races and fix flag ordering ([f9cbfde](https://github.com/louislafosse/hacksor/commit/f9cbfde3dc525fee817df547b75225da05e287d6))


### Features

* **bundle:** add Linux AppImage target ([1c42e69](https://github.com/louislafosse/hacksor/commit/1c42e69c962ff3bec487eab64b0af966817a15cf))
* **init:** initial public release of Hacksor ([fe9b91c](https://github.com/louislafosse/hacksor/commit/fe9b91c3018ea33f9b76ff3e3ae2548fe63e23de))
* **runtime:** implement detached spawning for CLI tools and enhance Docker connection handling ([dfbd73d](https://github.com/louislafosse/hacksor/commit/dfbd73d41510bcb1c396f3dcd508e75a56cae819))
* **ui:** show an install-Docker banner when Docker is missing ([29b8424](https://github.com/louislafosse/hacksor/commit/29b8424a5f14299ceffb793541a2baf243007509))

## [1.0.3](https://github.com/louislafosse/hacksor/compare/v1.0.2...v1.0.3) (2026-09-14)


### Bug Fixes

* **harness:** enable the update_plan tool ([a030008](https://github.com/louislafosse/hacksor/commit/a030008d1102ed75377b82512bf30ba777a0e6ce))
* **proxy:** detach mitmdump so it survives the starting shell ([958cc69](https://github.com/louislafosse/hacksor/commit/958cc6998d5a2102cc3d171804ee09cdb891e1f2))
* **runtime:** fix Windows Docker bind mount and improve status detection ([4699546](https://github.com/louislafosse/hacksor/commit/4699546005bf626ba47de688fbf402eb2f527fbe))
* **subagent:** retry transient config races and fix flag ordering ([e2e45c6](https://github.com/louislafosse/hacksor/commit/e2e45c675f6beb8e265406e8073089bcefcc44e6))

## [1.0.2](https://github.com/louislafosse/hacksor/compare/v1.0.1...v1.0.2) (2026-09-13)


### Bug Fixes

* **harness:** capture app-server stderr for crash diagnostics ([45fe5bc](https://github.com/louislafosse/hacksor/commit/45fe5bc5c53630b5208056385e0e4084e17170c2))

## [1.0.1](https://github.com/louislafosse/hacksor/compare/v1.0.0...v1.0.1) (2026-09-13)


### Bug Fixes

* **providers:** list models from ocx live catalog; open web dashboard ([ff5b90c](https://github.com/louislafosse/hacksor/commit/ff5b90c0cb8715a10da2a8391b1ed4d9b672c9be))

# 1.0.0 (2026-09-13)


### Bug Fixes

* **ci:** drop conflicting libappindicator3-dev; build macOS/Windows unsigned ([7c65f6c](https://github.com/louislafosse/hacksor/commit/7c65f6c478a8e1c7c54c88d54ee232b600fd8c83))
* **providers:** explain why a provider model list is empty ([fc9d360](https://github.com/louislafosse/hacksor/commit/fc9d360fe21c17fb9ab1813ce5a2c4d7556fa092))
* **providers:** sync + retry and sharper reason when a model list is empty ([d745da7](https://github.com/louislafosse/hacksor/commit/d745da7ef6135d4c19b6a8fd7bbeee42903f60c0))
* **runtime:** bridge ocx to the published port on Docker Desktop ([024d164](https://github.com/louislafosse/hacksor/commit/024d1643e5038e72ba2f275a431d1bc4c40db9cb))
* **runtime:** create/exec/remove containers via the docker CLI ([ec90f6b](https://github.com/louislafosse/hacksor/commit/ec90f6bde961ef77d34cdca13419904801c596ea))


### Features

* **bundle:** add Linux AppImage target ([1c42e69](https://github.com/louislafosse/hacksor/commit/1c42e69c962ff3bec487eab64b0af966817a15cf))
* **init:** initial public release of Hacksor ([fe9b91c](https://github.com/louislafosse/hacksor/commit/fe9b91c3018ea33f9b76ff3e3ae2548fe63e23de))
* **runtime:** implement detached spawning for CLI tools and enhance Docker connection handling ([dfbd73d](https://github.com/louislafosse/hacksor/commit/dfbd73d41510bcb1c396f3dcd508e75a56cae819))
* **ui:** show an install-Docker banner when Docker is missing ([29b8424](https://github.com/louislafosse/hacksor/commit/29b8424a5f14299ceffb793541a2baf243007509))

## [1.3.1](https://github.com/louislafosse/hacksor/compare/v1.3.0...v1.3.1) (2026-09-13)


### Bug Fixes

* **providers:** explain why a provider model list is empty ([fc9d360](https://github.com/louislafosse/hacksor/commit/fc9d360fe21c17fb9ab1813ce5a2c4d7556fa092))

# [1.3.0](https://github.com/louislafosse/hacksor/compare/v1.2.1...v1.3.0) (2026-09-13)


### Bug Fixes

* **runtime:** bridge ocx to the published port on Docker Desktop ([024d164](https://github.com/louislafosse/hacksor/commit/024d1643e5038e72ba2f275a431d1bc4c40db9cb))


### Features

* **ui:** show an install-Docker banner when Docker is missing ([29b8424](https://github.com/louislafosse/hacksor/commit/29b8424a5f14299ceffb793541a2baf243007509))

## [1.2.1](https://github.com/louislafosse/hacksor/compare/v1.2.0...v1.2.1) (2026-09-13)


### Bug Fixes

* **runtime:** create/exec/remove containers via the docker CLI ([ec90f6b](https://github.com/louislafosse/hacksor/commit/ec90f6bde961ef77d34cdca13419904801c596ea))

# [1.2.0](https://github.com/louislafosse/hacksor/compare/v1.1.0...v1.2.0) (2026-09-13)


### Features

* **runtime:** implement detached spawning for CLI tools and enhance Docker connection handling ([dfbd73d](https://github.com/louislafosse/hacksor/commit/dfbd73d41510bcb1c396f3dcd508e75a56cae819))

# [1.1.0](https://github.com/louislafosse/hacksor/compare/v1.0.1...v1.1.0) (2026-09-13)


### Features

* **bundle:** add Linux AppImage target ([1c42e69](https://github.com/louislafosse/hacksor/commit/1c42e69c962ff3bec487eab64b0af966817a15cf))

## [1.0.1](https://github.com/louislafosse/hacksor/compare/v1.0.0...v1.0.1) (2026-09-13)


### Bug Fixes

* **ci:** drop conflicting libappindicator3-dev; build macOS/Windows unsigned ([33ebf00](https://github.com/louislafosse/hacksor/commit/33ebf005df75dd6e94d515f1e077d46bd115c007))

# 1.0.0 (2026-09-13)


### Features

* **init:** initial public release of Hacksor ([fe9b91c](https://github.com/louislafosse/hacksor/commit/fe9b91c3018ea33f9b76ff3e3ae2548fe63e23de))
