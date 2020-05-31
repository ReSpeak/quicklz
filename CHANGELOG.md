# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2020-05-31
### ℹ Changed
- Switched error handling library from `failure` to `thiserror`
- Updated to Rust 2018 edition

### 🐛 Fixed
- Fixed possible corrupt header when compressing incompressible data with specific input lengths (found by libfuzz)
