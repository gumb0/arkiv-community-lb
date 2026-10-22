# The chain-writer sidecar: the TypeScript source on the pinned Node
# image, run as it is. Node strips the types itself, the same way the
# package scripts and CI run the code, so there is no build step and no
# dev dependency in the image. The base image is held still by digest.
FROM node:22.23.4-bookworm-slim@sha256:5991875dfb80938dd8a6ba3fe3e96fd04929e70792762b3319d8caadbbe229bc
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev && npm cache clean --force
COPY src ./src
USER node
# The endpoint, the key file and the listener come from the environment
# (see compose.yaml).
CMD ["node", "src/service.ts"]
