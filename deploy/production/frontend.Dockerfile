# syntax=docker/dockerfile:1.7
FROM node@sha256:a9f5f7c91a432850b2a8a7797adf5eadb6c733ceed61167806cee7ea7fbc29df AS console-build
WORKDIR /src
COPY console/package.json console/package-lock.json ./
RUN npm ci --ignore-scripts --no-audit --no-fund
COPY console/src ./src
COPY console/tests ./tests
COPY console/index.html console/tokens.css console/tsconfig.json console/vite.config.ts console/vitest.config.ts console/playwright.config.ts ./
RUN npm run build

FROM node@sha256:a9f5f7c91a432850b2a8a7797adf5eadb6c733ceed61167806cee7ea7fbc29df AS preview-build
ARG BROKERROUTER_CONSOLE_ORIGIN
WORKDIR /preview
COPY preview/review.html preview/review.js preview/review.css ./
RUN node --input-type=module -e 'import fs from "node:fs"; const raw=process.env.BROKERROUTER_CONSOLE_ORIGIN; const origin=new URL(raw); if(origin.protocol!=="https:" || origin.origin!==raw || origin.username || origin.password || origin.pathname!=="/" || origin.search || origin.hash || origin.port || !/^[a-z0-9]+(?:[.-][a-z0-9]+)*\.[a-z]{2,63}$/.test(origin.hostname)) throw Error("exact HTTPS DNS parent origin required"); const source=fs.readFileSync("review.html","utf8"); const marker="https://console.example.invalid"; if(source.split(marker).length!==2) throw Error("parent origin contract drift"); fs.writeFileSync("review.html",source.replace(marker,raw));'

FROM nginx@sha256:dc5069ad14f19660b141b21236140b91656bf89bbc3e2417c70ae650cd66104c AS console
COPY --from=console-build /src/dist /srv/brokerrouter/console
ENTRYPOINT ["nginx", "-g", "daemon off;"]

FROM nginx@sha256:dc5069ad14f19660b141b21236140b91656bf89bbc3e2417c70ae650cd66104c AS preview
ARG BROKERROUTER_CONSOLE_ORIGIN
LABEL io.brokerrouter.preview.parent-origin=$BROKERROUTER_CONSOLE_ORIGIN
COPY --from=preview-build /preview /srv/brokerrouter/preview
ENTRYPOINT ["nginx", "-g", "daemon off;"]
