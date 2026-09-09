# The chain-writer sidecar: the TypeScript source on the pinned Node
# image, run as it is — Node 22 strips the types itself, so there is no
# build step and no dev dependency in the image. The base image is held
# still by digest.
FROM node:22-bookworm-slim@sha256:83f487e0a63425e5b4d146fb5e5be574bcbe1b7b843d3ebafdd95eaf7767a7e5
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev && npm cache clean --force
COPY src ./src
USER node
# The endpoint, the key file and the listener come from the environment
# (see compose.yaml).
CMD ["node", "src/service.ts"]
