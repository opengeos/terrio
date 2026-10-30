# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/session.py

"""Cloud sessions: credentials for remote datasets, given to terrio as environment variables."""

from __future__ import annotations

import os
from typing import Any


class Session:
    """Base class of sessions: credential options for remote files."""

    @classmethod
    def hascreds(cls, config: dict[str, Any]) -> bool:
        """Whether a configuration holds credentials for this kind of session.

        Args:
            config: GDAL configuration options.

        Returns:
            bool: False in the base class.
        """
        return False

    def get_credential_options(self) -> dict[str, str]:
        """The credentials as GDAL configuration options.

        Returns:
            dict: Option names and values.
        """
        return {}

    @staticmethod
    def from_foreign_session(session: Any, cls: type[Session] | None = None) -> Session:
        """A session from a boto3 session.

        Args:
            session: A boto3 session.
            cls: The session class, `AWSSession` by default.

        Returns:
            Session: The session.
        """
        factory: Any = cls or AWSSession
        result: Session = factory(session=session)
        return result

    @staticmethod
    def cls_from_path(path: Any) -> type[Session]:
        """The session class for a path.

        Args:
            path: A path or URL.

        Returns:
            type: `AWSSession` for `s3://` paths, else `DummySession`.
        """
        s = str(path)
        if s.startswith("s3://") or s.startswith("/vsis3/"):
            return AWSSession
        if s.startswith("gs://") or s.startswith("/vsigs/"):
            return GSSession
        if s.startswith("az://") or s.startswith("/vsiaz/"):
            return AzureSession
        return DummySession

    @staticmethod
    def from_path(path: Any, *args: Any, **kwargs: Any) -> Session:
        """A session for a path.

        Args:
            path: A path or URL.
            *args: Session arguments.
            **kwargs: Session keyword arguments.

        Returns:
            Session: The session.
        """
        return Session.cls_from_path(path)(*args, **kwargs)

    @staticmethod
    def from_environ(*args: Any, **kwargs: Any) -> Session:
        """An AWS session from the environment.

        Args:
            *args: Session arguments.
            **kwargs: Session keyword arguments.

        Returns:
            Session: The session.
        """
        return AWSSession(*args, **kwargs)


class DummySession(Session):
    """A session without credentials."""

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        """Creates the session.

        Args:
            *args: Ignored.
            **kwargs: Ignored.
        """
        self.credentials: dict[str, str] = {}


class AWSSession(Session):
    """Amazon Web Services credentials, for `s3://` datasets."""

    def __init__(
        self,
        session: Any = None,
        aws_unsigned: bool | None = None,
        aws_access_key_id: str | None = None,
        aws_secret_access_key: str | None = None,
        aws_session_token: str | None = None,
        region_name: str | None = None,
        profile_name: str | None = None,
        endpoint_url: str | None = None,
        requester_pays: bool = False,
    ) -> None:
        """Creates the session.

        Args:
            session: A boto3 session to take credentials from.
            aws_unsigned: Make unsigned requests (public buckets).
            aws_access_key_id: The access key.
            aws_secret_access_key: The secret key.
            aws_session_token: The session token.
            region_name: The region.
            profile_name: A profile of the AWS configuration files.
            endpoint_url: An S3-compatible endpoint.
            requester_pays: Accept requester-pays charges.
        """
        if aws_unsigned is None:
            aws_unsigned = os.environ.get("AWS_NO_SIGN_REQUEST", "").upper() in ("YES", "TRUE")
        self.unsigned = bool(aws_unsigned)
        self.endpoint_url = endpoint_url
        self.requester_pays = requester_pays
        self.profile_name = profile_name
        self._creds: dict[str, str] = {}
        if session is not None:
            creds = session.get_credentials()
            if creds is not None:
                frozen = creds.get_frozen_credentials()
                aws_access_key_id = aws_access_key_id or frozen.access_key
                aws_secret_access_key = aws_secret_access_key or frozen.secret_key
                aws_session_token = aws_session_token or frozen.token
            region_name = region_name or getattr(session, "region_name", None)
        if aws_access_key_id:
            self._creds["aws_access_key_id"] = aws_access_key_id
        if aws_secret_access_key:
            self._creds["aws_secret_access_key"] = aws_secret_access_key
        if aws_session_token:
            self._creds["aws_session_token"] = aws_session_token
        if region_name:
            self._creds["aws_region"] = region_name

    @classmethod
    def hascreds(cls, config: dict[str, Any]) -> bool:
        """Whether a configuration holds AWS credentials.

        Args:
            config: GDAL configuration options.

        Returns:
            bool: True when it has an access key and secret key.
        """
        return "AWS_ACCESS_KEY_ID" in config and "AWS_SECRET_ACCESS_KEY" in config

    @property
    def credentials(self) -> dict[str, str]:
        """The credentials.

        Returns:
            dict: Lower-case names and values.
        """
        return dict(self._creds)

    def get_credential_options(self) -> dict[str, str]:
        """The credentials as GDAL configuration options (environment variables for terrio).

        Returns:
            dict: Option names and values.
        """
        if self.unsigned:
            opts = {"AWS_NO_SIGN_REQUEST": "YES"}
            if "aws_region" in self._creds:
                opts["AWS_REGION"] = self._creds["aws_region"]
        else:
            opts = {k.upper(): v for k, v in self._creds.items()}
        if self.profile_name:
            opts["AWS_PROFILE"] = self.profile_name
        if self.requester_pays:
            opts["AWS_REQUEST_PAYER"] = "requester"
        if self.endpoint_url:
            opts["AWS_S3_ENDPOINT"] = self.endpoint_url
        return opts


class GSSession(Session):
    """Google Cloud Storage credentials (`GOOGLE_APPLICATION_CREDENTIALS`)."""

    def __init__(self, google_application_credentials: str | None = None) -> None:
        """Creates the session.

        Args:
            google_application_credentials: The path of a credentials file.
        """
        self._creds = (
            {"google_application_credentials": google_application_credentials}
            if google_application_credentials
            else {}
        )

    @property
    def credentials(self) -> dict[str, str]:
        """The credentials.

        Returns:
            dict: Names and values.
        """
        return dict(self._creds)

    def get_credential_options(self) -> dict[str, str]:
        """The credentials as configuration options.

        Returns:
            dict: Option names and values.
        """
        return {k.upper(): v for k, v in self._creds.items()}


class AzureSession(Session):
    """Azure Blob Storage credentials."""

    def __init__(
        self,
        azure_storage_connection_string: str | None = None,
        azure_storage_account: str | None = None,
        azure_storage_access_key: str | None = None,
        azure_unsigned: bool = False,
        azure_storage_sas_token: str | None = None,
    ) -> None:
        """Creates the session.

        Args:
            azure_storage_connection_string: A connection string.
            azure_storage_account: The storage account.
            azure_storage_access_key: The access key.
            azure_unsigned: Make unsigned requests.
            azure_storage_sas_token: A SAS token.
        """
        self.unsigned = azure_unsigned
        self._creds = {
            k: v
            for k, v in {
                "azure_storage_connection_string": azure_storage_connection_string,
                "azure_storage_account": azure_storage_account,
                "azure_storage_access_key": azure_storage_access_key,
                "azure_storage_sas_token": azure_storage_sas_token,
            }.items()
            if v
        }

    @property
    def credentials(self) -> dict[str, str]:
        """The credentials.

        Returns:
            dict: Names and values.
        """
        return dict(self._creds)

    def get_credential_options(self) -> dict[str, str]:
        """The credentials as configuration options.

        Returns:
            dict: Option names and values.
        """
        opts = {k.upper(): v for k, v in self._creds.items()}
        if self.unsigned:
            opts["AZURE_NO_SIGN_REQUEST"] = "YES"
        return opts


__all__ = ["AWSSession", "AzureSession", "DummySession", "GSSession", "Session"]
