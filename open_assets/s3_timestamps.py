"""Parse canonical S3 UTC timestamps without a general date-language parser."""

import datetime
from functools import lru_cache
import re

UTC = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?Z")


def parse(value):
    if isinstance(value, str) and len(value) <= 27 and UTC.fullmatch(value):
        try:
            return datetime.datetime.fromisoformat(value)
        except ValueError:
            pass
    # Preserve botocore's behavior for HTTP dates, epochs and unknown formats.
    from botocore.utils import parse_timestamp

    return parse_timestamp(value)


@lru_cache(maxsize=1)
def session():
    """An application session with its own parser factory; no global SDK changes."""
    import boto3.session
    import botocore.session

    core = botocore.session.get_session()
    core.get_component("response_parser_factory").set_parser_defaults(
        timestamp_parser=parse
    )
    return boto3.session.Session(botocore_session=core)
