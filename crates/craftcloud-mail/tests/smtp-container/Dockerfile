# Floating test-only fixture image; never used by production or installation.
FROM python:3.12-alpine
RUN apk add --no-cache openssl
COPY smtp_server.py /opt/smtp_server.py
EXPOSE 2525
CMD ["python", "/opt/smtp_server.py"]
