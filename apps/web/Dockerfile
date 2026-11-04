# Build stage
FROM node:24-alpine AS build
WORKDIR /app
COPY package.json package-lock.json tsconfig.base.json ./
COPY apps/web/package.json apps/web/
RUN npm ci --workspace apps/web --include-workspace-root
COPY apps/web apps/web
ARG VITE_PLATFORM_URL=""
ENV VITE_PLATFORM_URL=${VITE_PLATFORM_URL}
WORKDIR /app/apps/web
RUN npm run build

# Serve stage
FROM nginx:alpine
COPY --from=build /app/apps/web/dist /usr/share/nginx/html
COPY apps/web/nginx.conf /etc/nginx/conf.d/default.conf
EXPOSE 80
