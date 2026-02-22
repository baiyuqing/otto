# syntax=docker/dockerfile:1

FROM --platform=$BUILDPLATFORM golang:1.25-bookworm AS builder

WORKDIR /src

COPY go.mod go.sum* ./
RUN go mod download

COPY . .
ARG TARGETOS
ARG TARGETARCH
RUN CGO_ENABLED=0 GOOS=${TARGETOS:-linux} GOARCH=${TARGETARCH} \
    go build -trimpath -ldflags='-s -w' -o /out/mysqlbench .

FROM --platform=$TARGETPLATFORM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        coreutils \
        curl \
        dnsutils \
        iputils-ping \
        mysql-client \
        netcat-openbsd \
        procps \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work

COPY --from=builder /out/mysqlbench /usr/local/bin/mysqlbench

ENTRYPOINT ["mysqlbench"]
CMD ["-h"]
