.PHONY: all build fmt lint test clean

# Default target
all: build

# Build the mysqlbench binary
build:
	go build -o mysqlbench .

# Format Go code
fmt:
	gofmt -w -s .

# Run Go static analysis
lint:
	go vet ./...

# Run test suite
test:
	go test -v ./...

# Remove build artifacts
clean:
	rm -f mysqlbench
