# Everything to cross-compile for Windows from Linux: Rust and MinGW stay here.
FROM registry.fedoraproject.org/fedora:44
RUN dnf -y --setopt=install_weak_deps=False install cargo rust rust-std-static-x86_64-pc-windows-gnu mingw64-gcc mingw64-winpthreads-static clang cmake nasm
ENV CARGO_HOME=/cargo CARGO_BUILD_TARGET=x86_64-pc-windows-gnu
