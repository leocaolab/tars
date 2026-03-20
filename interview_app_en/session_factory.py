"""EN session factory — reuses interview_app.session_factory with lang='en'."""

from pathlib import Path
from interview_app.session_factory import SessionFactoryV2 as _Base, load_rubric, load_blueprint, SessionFactory  # noqa: F401


class SessionFactoryV2(_Base):
    def __init__(self, base_dir: str = None):
        super().__init__(base_dir or str(Path(__file__).parent), lang="en")
