FROM rust:1-buster

COPY . .

RUN cargo install --path .

CMD ["hook"]
