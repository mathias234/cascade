# SSL/TLS Support for Cascade

This document describes the optional SSL/TLS module for Cascade that provides automatic certificate management via Let's Encrypt.

## Features

- Automatic certificate acquisition and renewal via Let's Encrypt
- Support for multiple domains
- Automatic HTTP-to-HTTPS redirect
- Zero-downtime certificate updates
- Production and staging environment support

## Building with SSL Support

To build Cascade with SSL support, use the `ssl` feature flag:

```bash
cd cascade
cargo build --release --features ssl
```

## Configuration

Add the following environment variables to your `.env` file:

```env
# Enable SSL (requires ssl feature)
SSL_ENABLED=true

# Comma-separated list of domains
SSL_DOMAINS=example.com,www.example.com

# Contact email for Let's Encrypt
SSL_EMAIL=admin@example.com

# Use staging environment for testing (recommended initially)
SSL_STAGING=true

# Certificate cache directory
SSL_CERT_CACHE_DIR=/etc/cascade/certs

# HTTPS port
SSL_PORT=443

# Redirect HTTP to HTTPS
HTTP_REDIRECT=true
```

## Testing

1. **Start with Staging Environment**: Always test with `SSL_STAGING=true` first to avoid rate limits.

2. **Domain Requirements**:
   - Domains must point to your server's IP address
   - Ports 80 and 443 must be accessible from the internet
   - Let's Encrypt needs to verify domain ownership

3. **Certificate Storage**: Certificates are cached in `SSL_CERT_CACHE_DIR`. Ensure this directory:
   - Has appropriate permissions
   - Is persistent (especially in Docker)
   - Is backed up in production

## Docker Deployment

The Docker configuration has been updated to support SSL:

```yaml
services:
  cascade:
    ports:
      - "80:80"      # HTTP (for ACME challenges and redirect)
      - "443:443"    # HTTPS
    volumes:
      - ./certs:/etc/cascade/certs  # Persistent certificate storage
```

## Production Checklist

Before switching to production (`SSL_STAGING=false`):

- [ ] Test thoroughly with staging certificates
- [ ] Verify domain DNS records point to your server
- [ ] Ensure firewall allows ports 80 and 443
- [ ] Set up certificate backup strategy
- [ ] Monitor Let's Encrypt rate limits
- [ ] Configure proper certificate cache directory permissions

## Rate Limits

Let's Encrypt has rate limits for production:
- 50 certificates per registered domain per week
- 5 duplicate certificates per week
- 300 new orders per account per 3 hours

Always use staging for testing to avoid hitting these limits.

## Troubleshooting

1. **Certificate not trusted**: Ensure `SSL_STAGING=false` for production certificates
2. **ACME challenges failing**: Check that port 80 is accessible
3. **Rate limit errors**: Switch to staging or wait for limits to reset
4. **Permission errors**: Ensure the certificate cache directory is writable

## Architecture

The SSL module uses `rustls-acme` v0.14 which provides:
- Automatic ACME protocol handling
- Built-in axum integration
- TLS-ALPN-01 challenge support
- Efficient certificate management

The implementation is modular and only compiled when the `ssl` feature is enabled, ensuring zero overhead for deployments that don't need SSL.