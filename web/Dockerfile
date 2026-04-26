# syntax=docker/dockerfile:1.7

FROM node:22-alpine AS builder
WORKDIR /build
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# Runtime: nginxinc/nginx-unprivileged is non-root by default and rootless-friendly.
FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY --from=builder /build/dist /usr/share/nginx/html
COPY web/nginx.conf /etc/nginx/conf.d/default.conf
EXPOSE 8080
