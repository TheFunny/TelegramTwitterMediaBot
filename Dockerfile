FROM python:3.13-slim-bullseye

LABEL maintainer="admin@yoursfunny.top"

ENV PYTHONDONTWRITEBYTECODE=1
ENV PYTHONUNBUFFERED=1

RUN set -eux; \
	apt-get update; \
	apt-get install -y git gosu; \
	rm -rf /var/lib/apt/lists/*; \
# verify that the binary works
	gosu nobody true

WORKDIR /app

COPY requirements.txt /app
RUN python -m pip install --no-cache-dir --upgrade -r requirements.txt

COPY . /app

RUN chmod a+x docker-entrypoint.sh

ENTRYPOINT ["/app/docker-entrypoint.sh"]

CMD ["python", "main.py"]
