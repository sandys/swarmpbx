# The carrier simulator of the e2e tests (diago). go.sum pins every module.
FROM golang:1.24-bookworm AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY *.go ./
RUN CGO_ENABLED=0 go build -trimpath -o /carrier-sim .

FROM debian:bookworm-slim
COPY --from=build /carrier-sim /usr/local/bin/carrier-sim
ENTRYPOINT ["carrier-sim"]
