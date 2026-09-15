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
