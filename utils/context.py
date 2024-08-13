import dataclasses
from typing import Optional

from telegram import Message
from telegram.ext import Application, CallbackContext, ExtBot


@dataclasses.dataclass
class EditMessage:
    url: str
    forward: tuple[Message, ...]


class ChatData:
    def __init__(self):
        self.forward_channel_id: Optional[int] = None
        self.edit_before_forward: bool = False
        self.edit_message: dict[int, EditMessage] = {}
        self.template: dict[str, str] = {}

    def __str__(self):
        return f"ChatData(forward_channel_id={self.forward_channel_id}, edit_before_forward={self.edit_before_forward}, " \
               f"edit_message={self.edit_message}, template={self.template})"


class CustomContext(CallbackContext[ExtBot, dict, ChatData, dict]):
    def __init__(
            self,
            application: Application,
            chat_id: Optional[int] = None,
            user_id: Optional[int] = None
    ):
        super().__init__(application=application, chat_id=chat_id, user_id=user_id)
