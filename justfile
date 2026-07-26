# Install kite into the local cargo bin directory.
install:
    cargo install --path .

# Build the project.
build:
    cargo build

# Check the project without producing binaries.
check:
    cargo check
