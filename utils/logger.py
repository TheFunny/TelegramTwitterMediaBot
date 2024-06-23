import logging
import os

logging.basicConfig(
    level=os.getenv("LOG_LEVEL", "WARNING"),
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)


def get_logger(name: str) -> logging.Logger:
    return logging.getLogger(name)
