<!--
 Licensed to the Apache Software Foundation (ASF) under one
 or more contributor license agreements.  See the NOTICE file
 distributed with this work for additional information
 regarding copyright ownership.  The ASF licenses this file
 to you under the Apache License, Version 2.0 (the
 "License"); you may not use this file except in compliance
 with the License.  You may obtain a copy of the License at

     http://www.apache.org/licenses/LICENSE-2.0

 Unless required by applicable law or agreed to in writing,
 software distributed under the License is distributed on an
 "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 KIND, either express or implied.  See the License for the
 specific language governing permissions and limitations
 under the License.
-->

# Apache Fluss Gateway image

The Gateway image follows the Java Fluss image conventions:

- installation root: `/opt/fluss`
- configuration directory: `/opt/fluss/conf`
- runtime user: `fluss` (`uid=9999`, `gid=9999`)
- REST port: `8080`
- Prometheus port: `9095`
- foreground process with `SIGTERM` graceful shutdown

For local development, build the native Linux distribution and image together:

```bash
GATEWAY_IMAGE=apache/fluss-gateway:dev docker/fluss-gateway/build.sh
```

The helper uses the pinned `rust:1.88-bookworm` builder, creates the Linux
binary distribution, prepares `build-target/<arch>/`, and then runs the
runtime-only Docker build. `RUN_SMOKE=true` also exercises the distribution
and image.

Release images are convenience artifacts. Prepare both voted binary
distributions before building a multi-platform image:

```bash
RELEASE_VERSION=1.0.0 \
  docker/fluss-gateway/prepare_build.sh

cd docker/fluss-gateway
docker buildx build \
  --push \
  --platform linux/amd64,linux/arm64 \
  --build-arg FLUSS_VERSION=1.0.0-rc1 \
  --build-arg VCS_REF="$(git rev-parse HEAD)" \
  --tag apache/fluss-gateway:1.0.0-rc1 \
  .
```

The Dockerfile selects `build-target/amd64` or `build-target/arm64` through
BuildKit's `TARGETARCH`; it does not compile Rust under QEMU.

Run against a Fluss cluster:

```bash
docker run --rm \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --stop-timeout 35 \
  -p 8080:8080 \
  -p 9095:9095 \
  -e FLUSS_GATEWAY__CLUSTER__DEFAULT__BOOTSTRAP__SERVERS=host.docker.internal:9123 \
  apache/fluss-gateway:dev
```

Use a cluster DNS name or container-network alias instead of
`host.docker.internal` on Linux deployments where that hostname is not
provided. The image does not require a writable root filesystem or Linux
capabilities.

Check the process endpoints:

```bash
curl http://127.0.0.1:8080/health
curl http://127.0.0.1:8080/ready
curl http://127.0.0.1:9095/metrics
```

Configuration uses the same flat YAML style as `conf/server.yaml`. Environment
variables override the file by removing the `gateway.` prefix, uppercasing the
segments, replacing hyphens with underscores, and joining them with `__`. For
example:

```text
gateway.rest.listen
FLUSS_GATEWAY__REST__LISTEN
```

Mount a replacement file and set `FLUSS_GATEWAY_CONFIG` when file-based secrets
or deployment-specific settings are required. The native distribution template
uses loopback, while the image sets
`FLUSS_GATEWAY__REST__LISTEN=0.0.0.0:8080` and
`FLUSS_GATEWAY__METRICS__EXPORTER__PROMETHEUS__LISTEN=0.0.0.0:9095`.
Override those variables when the mounted configuration must use different
listener addresses.

If the REST port is changed, also update `FLUSS_GATEWAY_HEALTH_URL` so the
container health check uses the same port.

The Gateway currently serves plain HTTP. Production deployments should use a
trusted ingress or reverse proxy for TLS and should not expose `trust`
authentication outside a protected network boundary. Expose the Prometheus
listener only to the monitoring network.

The configured shutdown drain timeout is 30 seconds. Use a container stop
timeout greater than that value:

```bash
docker stop --time 35 <container>
```

For Docker Compose, set `stop_grace_period: 35s`. For Kubernetes, set
`terminationGracePeriodSeconds` to at least `35`.

Build and run the image smoke test with:

```bash
RUN_SMOKE=true docker/fluss-gateway/build.sh
```
