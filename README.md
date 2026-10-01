# cargo-hole

> 用「规格」代替实现：找出 Rust 代码里带 `spec:` 的 `todo!()`，交给模型补全，
> 再用 rustc 自己验收。

```rust
pub fn resize(img: &Image) -> Image {
    todo!("spec: 缩放到最长边不超过 1024px，保持宽高比，不放大；1920x1080 -> 1024x576")
}
```

```bash
$ cargo hole list --path .
$ cargo hole fill --path .          # 生成到 .cargo-hole/，源码不动
```

## 原理

想法来自 [硅基天启：灭世之技术推演（ChinaSys'25 Winter）](https://www.bilibili.com/video/BV1KEinBtEU6/)
里「消灭码农」的那一问：不直接写实现，只写规格（「混合语言手册」），由模型把规格展开成项目。
cargo-hole 是这条路线的最小可用闭环 —— **规格写在编译器看得见的地方，生成结果由编译器验收**。

**1. 空洞 = 带规格的 `todo!`。**
`todo!("spec: ...")` / `unimplemented!("spec: ...")` 就是空洞；`spec:` 前缀是显式开关，
不带前缀的 `todo!()` 不是空洞。规格文本就是这次要实现的接口：写在函数体里、离签名和上下文最近。

**2. rustc 本身就是类型查询 API。**
`todo!()` 的类型是 `!`，可以强制转换成任何类型，所以它自己永远不会触发类型错误。
把空洞换成 `()`，rustc 就会告诉你那个位置原本期望什么类型：

```
E0308: expected `Image`, found `()`
```

于是一次 `cargo check` 就换来期望类型、精确 span 和真实的推断结果（含强制转换）——这正是
提示模型所需要的上下文。探针是批量的：一个文件里的所有空洞一次性替换，每条 `E0308` 自带 span，
所以一次检查能回答全部空洞；批量答不了的再单独探测，因此批量结果不会弱于逐个探测。
`--no-probe` 可关掉，此时模型只拿到规格文本和空洞的语法位置。

**3. 编译门禁决定什么可以进账本。**
生成的整棵代码树必须先通过一次 `cargo check` 才会被写出。失败时，错误会映射回它落在哪个空洞的
生成代码里，`fill` 只对这几个空洞重问（并把 rustc 的报错原文一起给它）。
所以账本里没有一行是没编译过的代码；被门禁拒绝的一轮什么都不会留下，`--in-place` 也不会有
"先写入再回滚"的窗口。`--no-verify` 跳过门禁，代价是**这一轮不记录任何缓存**。

## 安装

需要支持 edition 2024 的 Rust（1.85+）。

```bash
git clone <this repo> && cd cargo-hole
cargo install --path .      # 安装 cargo-hole 到 ~/.cargo/bin，之后 `cargo hole` 可用
```

## 使用

```bash
cargo hole list  --path .                 # 列出所有空洞：位置、状态、所属函数、规格
cargo hole list  --path . --pretty        # 按文件分组、完整换行显示，便于阅读
cargo hole list  --path . --fail-on-unelaborated   # 还有空洞就以 1 退出，可放进 CI

cargo hole fill  --path .                 # 让模型填补，结果写到 .cargo-hole/patch/
cargo hole fill  --path . --in-place      # 直接写回源文件（仍然先过门禁）
cargo hole fill  --path . --no-probe      # 不探测期望类型，只给模型规格和位置

cargo hole build --path .                 # 在 .cargo-hole/build/ 里编译生成后的 crate
cargo hole build --path . --release       # 命令之后的参数原样传给 cargo
cargo hole run   --path .                 # 编译并运行
cargo hole run   --path . -- a b          # `--` 之后的参数给程序本身
```

公共约定：

- `--path` 是要处理的 crate 根目录，默认 `.`，会被 canonicalize；每个命令都接受。
- 只有 `spec:` 空洞会被填补；写在空洞上方连续 `//` 注释里的 `// hole:pinned`
  会把空洞标成 `PINNED`，永不填补，也不会被缓存覆盖 —— 手写实现不想被模型动时用它。
- 状态有三种：`open`、`PINNED`、`UNRESOLVABLE`。
- `list` 的默认输出是给脚本解析的；`--pretty` 才是给人读的，`--color auto|always|never` 控制颜色
  （颜色从不作为唯一信号，`NO_COLOR` 生效）。

`fill` 的运行摘要：

```
$ cargo hole fill --path .
filling 2 hole(s) in . using cli:codex (its own model)

2 filled, 0 cached, 0 not filled, 0 skipped (2 model call(s))
```

第二次 `fill` 未改动的 crate 不需要任何模型调用（`2 cached`）：答案记在账本里，键是空洞
**含义**的 blake3 哈希（规整空白后的规格文本、签名、`impl`/`trait` 上下文、语法位置），
行号不在其中 —— 上面插一行、调换函数顺序、跑 `cargo fmt` 都不会失效；改规格或签名才会。

## 生成物

```
.cargo-hole/
├── patch/         生成后的代码，镜像源码树
├── ledger.jsonl   每个通过编译门禁的答案（追加写，按空洞含义的哈希索引）
└── build/         影子构建树，派生自 patch/，随时可删
```

`patch/` 是产物，`build/` 是派生物，两者分开，命令不必猜自己在看哪一个。
`build` 不是 `cd .cargo-hole && cargo build`：那里没有 `Cargo.toml`，也没有无空洞文件的副本，
所以它是把整个 crate 镜像到 `.cargo-hole/build/`、再把生成结果覆盖（软链接）上去，在副本里跑 cargo ——
源文件全程不被写入。请把 `.cargo-hole/build/` 加进 `.gitignore`。

注意：未填补的空洞仍是 `todo!()`，类型是 `!`，所以**满是空洞的 crate 也能编译**。
`build` 会显式警告还有多少空洞没填，而不是报一个干净的 success。

## 配置

设置分层叠加，越靠后优先级越高：内置默认值 → crate 根目录的 `.cargo-hole.toml` → `CARGO_HOLE_*`
环境变量 → 命令行参数。

```toml
# .cargo-hole.toml（放在 --path 指向的目录，而不是当前工作目录）
[model]
model       = ""                    # 留空表示用 CLI 自己的配置
base_url    = "http://127.0.0.1:11434/v1"
api_key_env = "MY_API_KEY"          # 推荐：文件可以安全提交
api_key     = "sk-literal"          # 也支持，两者同时存在时 api_key_env 优先

[fill]
max_attempts = 3
max_tokens   = 2048
temperature  = 0.0
```

| 环境变量 | 作用 |
|---|---|
| `CARGO_HOLE_CLI` | 驱动的 agent CLI（目前只认 `codex`） |
| `CARGO_HOLE_MODEL` | 模型名 |
| `CARGO_HOLE_BASE_URL` | base URL |
| `CARGO_HOLE_API_KEY` | API key |
| `CARGO_HOLE_MAX_ATTEMPTS` | 每个空洞的尝试次数 |
| `CARGO_HOLE_MAX_TOKENS` | 生成 token 上限 |
| `CARGO_HOLE_TEMPERATURE` | 采样温度 |

空变量/空参数会被忽略而不是覆盖已有的值；解析失败的值会告警并丢弃；config 文件里出现未知键时
整个文件被丢弃（并提示可接受的键名），不会半途生效。

## 当前状态

已实现：空洞发现与列出、规格解析、`// hole:pinned`、语法位置判定、`codex` agent、失败重试、
配置分层、写出到 `.cargo-hole/` 或 `--in-place`、缓存账本、批量类型探针、编译门禁、
`build` 的影子构建树、`run`。

尚未实现（因此没有任何东西依赖它们）：

- `cargo hole restore` —— 子命令存在但直接 `unimplemented!()` panic；探针锁、`.cargo-hole.bak`
  和 `restore_leftovers` 都已写好并测试过，只是 CLI 没有调用。
- `--at <file>:<line>` —— 被 CLI 接受但被忽略，仍然填补所有空洞。
- Provider 可插拔 —— 只有设计草案 `docs/providers.md`，没有 `Provider` trait / `script` provider；
  唯一可用的 agent 是 `codex` CLI。相应地 `base_url`、`api_key`、`max_tokens`、`temperature`
  会被解析和分层，但目前无人读取。
- `#[hole::pin]` / `#[pin]` 写在**外层**函数、方法、`impl` 或 `trait` 上时不生效（栈被遮蔽的
  bug，`has_pin_attribute` 与文档承诺的行为不符）。在修好之前，把属性直接写在 `todo!` 上，
  或者用 `// hole:pinned`。
- `cargo hole type` —— 只报告空洞期望类型而不填补的命令；探针已经能回答，只是没暴露。

## 开发与文档

```bash
cargo test      # 248 个测试，全部离线，不需要网络
cargo build
```

- [`docs/design.md`](docs/design.md) —— 详细设计说明（原 README，英文）：每条命令背后的取舍、
  门禁与账本的契约、类型探针拦不到的边界情况、模块职责表、已知问题。
- [`docs/providers.md`](docs/providers.md) —— Provider 插件的设计草案（尚未实现）。

模块划分：`hole.rs`（syn 提取空洞）、`cmdline.rs`（CLI）、`filler.rs`（提示词与拼接）、
`storage*.rs`（账本）、`render.rs`（list 输出）、`agent*.rs`（配置分层与 codex）、
`prober.rs`（类型探针）、`verifier.rs`（编译门禁）、`shadow.rs`（影子构建树）、`util.rs`。
