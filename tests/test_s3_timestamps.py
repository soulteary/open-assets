"""Canonical S3 dates retain exact standard-parser semantics and a safe fallback."""

import datetime
import unittest
from unittest.mock import patch

from botocore.utils import parse_timestamp
from open_assets import s3_timestamps


class S3Timestamps(unittest.TestCase):
    def test_canonical_utc_keeps_microseconds_without_general_parser(self):
        for value in (
            "2026-11-09T11:15:30Z",
            "2024-02-29T23:59:59.1Z",
            "2026-11-09T11:15:30.123456Z",
        ):
            expected = parse_timestamp(value)
            with (
                self.subTest(value=value),
                patch(
                    "botocore.utils.parse_timestamp",
                    side_effect=AssertionError("general parser used"),
                ),
            ):
                self.assertEqual(s3_timestamps.parse(value), expected)
                self.assertEqual(
                    s3_timestamps.parse(value).utcoffset(), datetime.timedelta(0)
                )

    def test_other_supported_formats_use_original_parser(self):
        for value in (
            "Mon, 09 Nov 2026 11:15:30 GMT",
            "2026-11-09T13:15:30+02:00",
            "2026-11-09T11:15:30.1234567Z",
            1794222930,
            1794222930.25,
        ):
            expected = parse_timestamp(value)
            with (
                self.subTest(value=value),
                patch(
                    "botocore.utils.parse_timestamp", wraps=parse_timestamp
                ) as fallback,
            ):
                self.assertEqual(s3_timestamps.parse(value), expected)
                fallback.assert_called_once_with(value)

    def test_invalid_dates_remain_rejected(self):
        for value in ("2026-02-30T11:15:30Z", "2026-11-09T25:15:30Z", "not a date"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                s3_timestamps.parse(value)

    def test_scoped_factory_parses_real_s3_response_shape(self):
        import boto3

        before = boto3.DEFAULT_SESSION
        session = s3_timestamps.session()
        self.assertIs(session, s3_timestamps.session())
        client = session.client(
            "s3",
            region_name="us-east-1",
            endpoint_url="https://store.invalid",
            aws_access_key_id="fixture",
            aws_secret_access_key="fixture",
        )
        try:
            shape = client.meta.service_model.operation_model(
                "ListObjectsV2"
            ).output_shape
            parser = session._session.get_component(
                "response_parser_factory"
            ).create_parser("rest-xml")
            body = b'<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><IsTruncated>false</IsTruncated><Contents><Key>a</Key><LastModified>2026-11-09T11:15:30.123Z</LastModified><ETag>"abcdef"</ETag><Size>1</Size></Contents></ListBucketResult>'
            with patch(
                "botocore.utils.parse_timestamp",
                side_effect=AssertionError("general parser used"),
            ):
                response = parser.parse(
                    dict(
                        body=body,
                        status_code=200,
                        headers={"content-type": "application/xml"},
                    ),
                    shape,
                )
            self.assertEqual(
                response["Contents"][0]["LastModified"],
                datetime.datetime(
                    2026, 11, 9, 11, 15, 30, 123000, tzinfo=datetime.timezone.utc
                ),
            )
            self.assertIs(boto3.DEFAULT_SESSION, before)
        finally:
            client.close()
