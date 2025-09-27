# Njalla Webhook Provider for External-DNS

A Rust-based webhook provider that enables [external-dns](https://github.com/kubernetes-sigs/external-dns) to manage DNS records on [Njalla](https://njal.la) domains.

## Features

- 🦀 **Written in Rust** - Fast, safe, and efficient
- 🔐 **Njalla API Integration** - Full support for Njalla's JSON-RPC 2.0 API
- 🎯 **External-DNS Compatible** - Implements the webhook provider specification
- 🐳 **Docker Support** - Multi-stage builds with Nix for minimal images
- 📦 **Nix Flakes** - Reproducible builds and development environment
- 🚀 **GitHub Actions** - Automated CI/CD with container registry publishing
- 🔍 **Comprehensive Logging** - Structured logging with tracing
- ⚡ **Async/Await** - High-performance async operations with Tokio

## Quick Start

### Using Docker

```bash
docker run -d \
  -e NJALLA_API_TOKEN=your-token-here \
  -e DOMAIN_FILTER=example.com,example.org \
  -p 127.0.0.1:8888:8888 \
  ghcr.io/yourusername/njalla-webhook:latest
```

### Using Nix

```bash
# Run directly
nix run github:yourusername/njalla-webhook

# Build Docker image
nix build .#dockerImage
docker load < result
```

### From Source

```bash
# Clone the repository
git clone https://github.com/yourusername/njalla-webhook
cd njalla-webhook

# Enter development environment
nix develop

# Run the webhook
cargo run
```

## Configuration

Configure via environment variables:

| Variable | Description | Default | Required |
|----------|-------------|---------|----------|
| `NJALLA_API_TOKEN` | Njalla API token from njal.la/settings/api/ | - | Yes |
| `WEBHOOK_HOST` | Host to bind the webhook server | `127.0.0.1` | No |
| `WEBHOOK_PORT` | Port for the webhook server | `8888` | No |
| `DOMAIN_FILTER` | Comma-separated list of allowed domains | All domains | No |
| `DRY_RUN` | Enable dry-run mode (no actual changes) | `false` | No |
| `CACHE_TTL_SECONDS` | DNS cache TTL in seconds | `60` | No |
| `RUST_LOG` | Log level (trace, debug, info, warn, error) | `info` | No |

### Example .env file

```env
NJALLA_API_TOKEN=your-njalla-api-token-here
WEBHOOK_HOST=127.0.0.1
WEBHOOK_PORT=8888
DOMAIN_FILTER=example.com,example.org
RUST_LOG=info
DRY_RUN=false
```

## Kubernetes Deployment

### 1. Deploy the Webhook with External-DNS

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: njalla-webhook-secret
  namespace: external-dns
stringData:
  njalla-api-token: "your-token-here"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: external-dns
  namespace: external-dns
spec:
  replicas: 1
  selector:
    matchLabels:
      app: external-dns
  template:
    metadata:
      labels:
        app: external-dns
    spec:
      serviceAccountName: external-dns
      containers:
      # External-DNS container
      - name: external-dns
        image: registry.k8s.io/external-dns/external-dns:v0.14.0
        args:
          - --source=ingress
          - --source=service
          - --provider=webhook
          - --webhook-provider-url=http://localhost:8888
          - --interval=30s
          - --log-level=info
        resources:
          limits:
            memory: 256Mi
          requests:
            memory: 128Mi
            cpu: 50m

      # Njalla Webhook sidecar
      - name: njalla-webhook
        image: ghcr.io/yourusername/njalla-webhook:latest
        env:
        - name: NJALLA_API_TOKEN
          valueFrom:
            secretKeyRef:
              name: njalla-webhook-secret
              key: njalla-api-token
        - name: DOMAIN_FILTER
          value: "example.com,example.org"
        - name: RUST_LOG
          value: "info"
        ports:
        - containerPort: 8888
          name: webhook
        livenessProbe:
          httpGet:
            path: /healthz
            port: 8888
          initialDelaySeconds: 10
          periodSeconds: 30
        readinessProbe:
          httpGet:
            path: /ready
            port: 8888
          initialDelaySeconds: 5
          periodSeconds: 10
        resources:
          limits:
            memory: 128Mi
          requests:
            memory: 64Mi
            cpu: 50m
```

### 2. Create RBAC Resources

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: external-dns
  namespace: external-dns
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: external-dns
rules:
- apiGroups: [""]
  resources: ["services", "endpoints", "pods"]
  verbs: ["get", "watch", "list"]
- apiGroups: ["extensions", "networking.k8s.io"]
  resources: ["ingresses"]
  verbs: ["get", "watch", "list"]
- apiGroups: [""]
  resources: ["nodes"]
  verbs: ["list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: external-dns-viewer
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: external-dns
subjects:
- kind: ServiceAccount
  name: external-dns
  namespace: external-dns
```

### 3. Test with an Ingress

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: example-ingress
  annotations:
    external-dns.alpha.kubernetes.io/hostname: app.example.com
spec:
  rules:
  - host: app.example.com
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: example-service
            port:
              number: 80
```

## API Endpoints

The webhook implements the external-dns webhook specification:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/healthz` | GET | Health check endpoint |
| `/ready` | GET | Readiness check (validates Njalla API connection) |
| `/records` | GET | List DNS records for a zone |
| `/records` | POST | Apply DNS record changes (create/update/delete) |
| `/adjustendpoints` | POST | Adjust endpoints (optional, returns as-is) |

## Development

### Prerequisites

- [Nix](https://nixos.org/download.html) with flakes enabled
- Or manually install: Rust 1.70+, OpenSSL dev libraries

### Development Environment

```bash
# Enter the Nix development shell
nix develop

# Run with auto-reload
cargo watch -x run

# Run tests
cargo test

# Format code
cargo fmt

# Run clippy
cargo clippy -- -D warnings

# Build release binary
cargo build --release
```

### Project Structure

```
njalla-webhook/
├── src/
│   ├── main.rs           # Application entry point
│   ├── config.rs         # Configuration management
│   ├── error.rs          # Error types and handling
│   ├── njalla/           # Njalla API client
│   │   ├── client.rs     # HTTP client implementation
│   │   └── types.rs      # API types
│   └── webhook/          # Webhook server
│       ├── handlers.rs   # Request handlers
│       ├── routes.rs     # Route definitions
│       └── types.rs      # External-DNS types
├── flake.nix             # Nix flake configuration
├── Cargo.toml            # Rust dependencies
└── .github/workflows/    # CI/CD pipelines
```

## Building

### Local Build

```bash
# Development build
cargo build

# Release build (optimized)
cargo build --release

# Static binary with musl
nix build .#default
```

### Docker Build

```bash
# Build with Nix
nix build .#dockerImage
docker load < result

# Run the container
docker run -e NJALLA_API_TOKEN=your-token njalla-webhook:latest
```

## Testing

```bash
# Run all tests
cargo test

# Run with logging
RUST_LOG=debug cargo test -- --nocapture

# Test specific module
cargo test njalla::

# Integration tests (requires API token)
NJALLA_API_TOKEN=your-token cargo test --test '*'
```

## Contributing

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Make your changes
4. Run tests and formatting (`cargo test && cargo fmt`)
5. Commit your changes (`git commit -m 'Add amazing feature'`)
6. Push to the branch (`git push origin feature/amazing-feature`)
7. Open a Pull Request

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- [External-DNS](https://github.com/kubernetes-sigs/external-dns) for the webhook specification
- [Njalla](https://njal.la) for their privacy-focused domain services
- [Rust](https://www.rust-lang.org/) and the async ecosystem

## Support

- **Issues**: [GitHub Issues](https://github.com/yourusername/njalla-webhook/issues)
- **Discussions**: [GitHub Discussions](https://github.com/yourusername/njalla-webhook/discussions)
- **Documentation**: [External-DNS Webhook Docs](https://kubernetes-sigs.github.io/external-dns/latest/tutorials/webhook-provider/)