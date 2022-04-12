# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.1] - 2022-04-12
### 🐛 Fixed
- Fixed a bug in decompress, where the high part of the offset was not shifted by the correct number
  of bits, resulting in the wrong part of the data being copied
- Fixed a bug in compress, where it used an encoding even if the offset value was too large,
  resulting in the high bits being cut off

## [0.3.0] - 2020-05-31
### ℹ Changed
- Switched error handling library from `failure` to `thiserror`
- Updated to Rust 2018 edition

### 🐛 Fixed
- Fixed possible corrupt header when compressing incompressible data with specific input lengths (found by libfuzz)
