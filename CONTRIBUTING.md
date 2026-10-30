# Contributing to Hermes Air 🍃

Thank you for your interest in contributing to **Hermes Air**! We welcome community contributions, bug fixes, feature requests, and performance optimizations.

---

## 🛠️ Development Setup

Hermes Air is written in 100% asynchronous, pure Rust.

### Prerequisites
- **Rust Toolchain**: `rustc` and `cargo` (edition 2021, Rust 1.75+)
- **System Packages**: `openssl`, `pkg-config` (Linux)

### Clone & Build
```bash
git clone https://github.com/gb99xbear/hermes-air.git
cd hermes-air

# Check workspace compilation
cargo check --workspace

# Run complete automated test suite
cargo test --workspace
```

---

## 📐 Contribution Guidelines

1. **Keep it Featherlight ("Air" Ethos)**:
   - Hermes Air prioritizes near-zero memory footprint (~20MB RSS) and high asynchronous concurrency.
   - Avoid heavy runtime dependencies or unbounded in-memory buffers.

2. **1-to-1 Functional Parity with Hermes Agent**:
   - New agent tools and platform adapters should follow the reference specifications of the official Python Hermes Agent.
   - All tool schemas must adhere strictly to OpenAI function-calling standards.

3. **Strict Anonymity & Open-Source Cleanliness**:
   - Do **NOT** commit hardcoded private IP addresses, personal usernames, server tokens, or API credentials into source code or test fixtures.
   - All external endpoints must be configurable via environment variables or `config.toml`.

4. **Testing is Mandatory**:
   - Every bug fix or new tool implementation must be accompanied by unit tests and integration tests.
   - Run `cargo test --workspace` to ensure all 130+ automated tests pass with zero failures.

---

## 🚀 Submitting a Pull Request

1. Fork the repository and create your feature branch:
   ```bash
   git checkout -b feature/my-new-feature
   ```
2. Commit your changes with clear, descriptive commit messages:
   ```bash
   git commit -m "feat(tools): add new search provider support"
   ```
3. Push to your branch and open a Pull Request against the `main` branch:
   ```bash
   git push origin feature/my-new-feature
   ```

We appreciate your contributions to making autonomous AI agents lighter, faster, and accessible to everyone!
