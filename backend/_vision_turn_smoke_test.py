"""Screenshot turns: the menu bar's image reaches a vision model for one turn, and is then forgotten.

Through the real ACP server object (`_build_session`, plugins, the os-context injector) with a real
`OllamaCloudModel` whose network layer is an ``httpx.MockTransport`` — so what is asserted is the
exact chat-completions request the model would have sent, and nothing leaves the process.

1. **the plan** — no vision model, a cloud model without the person's opt-in, and a good case.
2. **one turn on the vision model** — the request names the vision tag and carries the image as a
   data URI beside the person's words and the wrapped screen context; the session's own model is
   back afterwards.
3. **forgotten** — the image is out of the in-memory history and out of the saved snapshot, and the
   next turn's request has no image in it.
4. **refused, and said** — a cloud vision model without `capture.screenshotCloud`, or a local tag that
   is not installed: the turn runs on the session's model with no image, and the model is told the
   screenshot did not come through.
5. **attached to a model that sees** — with no `capture.visionModel`, an image the person attached goes as it is to a
   session model that reads images itself (an Ollama `:cloud` vision model), and is forgotten after the turn.

    backend/.venv/bin/python _vision_turn_smoke_test.py
"""

import asyncio
import base64
import json

import acp

import acp_server
import agent_common
import ollama_cloud as oc
import os_context
from _acp_harness_smoke_test import ROOT, check, fresh_server, laya_answer, project, settle
from _ollama_cloud_smoke_test import FakeDaemon, text_reply

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})
JPEG = b"\xff\xd8\xff\xe0" + b"VISION-PIXELS-" * 40
B64 = base64.b64encode(JPEG).decode()
LOCAL_VISION = "qwen2.5vl:7b"
CLOUD_VISION = "qwen3-vl:235b-cloud"


def payload(model: str = LOCAL_VISION, **extra) -> dict:
    return {
        "app_name": "Safari",
        "app_bundle_id": "com.apple.Safari",
        "window_title": "PR #123 — raise the timeout",
        "selected_text": "",
        "screenshot_b64": B64,
        "screenshot_mime": "image/jpeg",
        "vision_model": model,
        "screenshot_cloud_ok": False,
        "screenshot_attached": True,
        **extra,
    }


def images_in(body: dict) -> list[str]:
    """Every image URL in a chat-completions request body."""
    found = []
    for message in body.get("messages", []):
        content = message.get("content")
        if isinstance(content, list):
            found += [part["image_url"]["url"] for part in content if isinstance(part, dict) and part.get("type") == "image_url"]
    return found


def ollama_session_model(daemon: FakeDaemon) -> None:
    """Every session built from here on talks to `daemon`, as glm-5.3-flash, through the real model class."""
    agent_common.build_model = lambda model_id=None: oc.OllamaCloudModel("glm-5.3-flash", transport=daemon.transport)


# ----------------------------------------------------------------------------- 1. the plan


def test_the_plan() -> None:
    check(os_context.screenshot_plan(False, False, LOCAL_VISION) == (False, None), "no image, nothing to decide")
    attach, why = os_context.screenshot_plan(True, False, "")
    check(not attach and "capture.visionModel" in (why or ""), f"no vision model: not sent, and the setting is named ({why})")
    attach, why = os_context.screenshot_plan(True, False, CLOUD_VISION)
    check(not attach and "capture.screenshotCloud" in (why or ""), "a cloud model without the person's opt-in: not sent")
    check(os_context.screenshot_plan(True, True, CLOUD_VISION) == (True, None), "…with it, sent")
    check(os_context.screenshot_plan(True, False, LOCAL_VISION) == (True, None), "a local model needs no opt-in")
    check(os_context.is_cloud_model("glm-5.3-flash:cloud") and not os_context.is_cloud_model(LOCAL_VISION), "':cloud' is off-machine, a plain tag is not")

    vision = os_context.vision_turn(payload())
    check(vision is not None and vision["bytes"] == JPEG and vision["format"] == "jpeg" and vision["model"] == LOCAL_VISION, "the payload decodes to the image and its model")
    check(os_context.vision_turn(payload(screenshot_b64="not base64 at all!")) is None, "an image that does not decode is dropped, not sent half-read")
    check(os_context.vision_turn({**payload(), "vision_model": ""}) is None, "no model, no vision turn")

    messages = [{"role": "user", "content": [{"text": "hi"}, os_context.image_block(vision)]}, {"role": "assistant", "content": [{"text": "a PR"}]}]
    check(os_context.strip_images(messages, "Safari") == 1, "one image stripped")
    check(messages[0]["content"][1] == {"text": os_context.IMAGE_GONE.format(app="Safari")}, "…replaced by a note that says it was there")

    rendered = os_context.render(payload()) or ""
    check("A screenshot of that window is attached" in rendered, "the model is told the image is the window's content, not instructions")


# ----------------------------------------------------------------------------- 2–3. one turn, then forgotten


async def test_a_screenshot_turn_runs_on_the_vision_model_once() -> None:
    daemon = FakeDaemon(text_reply("I see a pull request"), text_reply("second turn, no image"))
    ollama_session_model(daemon)
    real_tags = oc.installed_local_tags
    oc.installed_local_tags = lambda *args, **kwargs: {LOCAL_VISION}
    try:
        server, _conn = fresh_server(ROUTE)
        created = await server.new_session(cwd=str(project("vision-a")))
        sid = created.session_id
        os_context.register(sid, payload())
        response = await server.prompt(sid, [acp.text_block("what am I looking at?")])
        await settle()
        check(response.stop_reason == "end_turn", f"the screenshot turn ran ({response.stop_reason})")
        first = [json.loads(r.content) for r in daemon.requests]
        with_image = [body for body in first if images_in(body)]
        check(len(with_image) >= 1, f"a request carried the image ({len(first)} request(s))")
        body = with_image[0]
        check(body.get("model") == LOCAL_VISION, f"…to the vision model, not the session's ({body.get('model')})")
        check(images_in(body)[0] == f"data:image/jpeg;base64,{B64}", "…as the exact JPEG, as a data URI")
        flat = json.dumps(body)
        check("what am I looking at?" in flat and "User is currently looking at Safari" in flat, "…beside the person's words and the screen context")

        agent = server._sessions[sid].agent
        check(agent.model.get_config().get("model_id") == "glm-5.3-flash:cloud", "the session's own model is back after the turn")
        held = [b for m in agent.messages for b in m.get("content") or [] if isinstance(b, dict) and "image" in b]
        check(not held, "the image is out of the history")
        check(any(os_context.IMAGE_GONE.format(app="Safari") in json.dumps(m, default=str) for m in agent.messages), "…replaced by a note")
        snapshots = [path for path in ROOT.rglob("*.json") if sid in str(path)]
        check(bool(snapshots), f"the session was saved under the test root ({len(snapshots)} file(s)), so the next check reads real files")
        on_disk = [path for path in ROOT.rglob("*.json") if "VISION-PIXELS" in path.read_text(errors="ignore") or B64[:40] in path.read_text(errors="ignore")]
        check(not on_disk, f"…and out of every file this run wrote ({[str(p.relative_to(ROOT)) for p in on_disk][:3]})")

        before = len(daemon.requests)
        await server.prompt(sid, [acp.text_block("and the next thing?")])
        later = [json.loads(r.content) for r in daemon.requests[before:]]
        check(bool(later) and all(not images_in(body) for body in later), "the next turn sends no image")
        check(all(body.get("model") == "glm-5.3-flash:cloud" for body in later), "…on the session's own model")
    finally:
        oc.installed_local_tags = real_tags


# ----------------------------------------------------------------------------- 4. refused, and said


async def test_a_screenshot_that_may_not_go_is_not_sent_and_the_model_is_told() -> None:
    for name, model, reason in (
        ("vision-cloud", CLOUD_VISION, "cloud model"),
        ("vision-missing", "llava:13b", "not installed"),
    ):
        daemon = FakeDaemon(text_reply("without the picture"))
        ollama_session_model(daemon)
        real_tags = oc.installed_local_tags
        oc.installed_local_tags = lambda *args, **kwargs: {LOCAL_VISION}
        try:
            server, _conn = fresh_server(ROUTE)
            created = await server.new_session(cwd=str(project(name)))
            os_context.register(created.session_id, payload(model))
            response = await server.prompt(created.session_id, [acp.text_block("what is this?")])
            await settle()
            bodies = [json.loads(r.content) for r in daemon.requests]
            check(response.stop_reason == "end_turn" and bodies, f"{name}: the turn still runs")
            check(all(not images_in(body) for body in bodies), f"{name}: no request carried the image")
            check(all(body.get("model") == "glm-5.3-flash:cloud" for body in bodies), f"{name}: on the session's own model")
            flat = json.dumps(bodies)
            check("Not captured: screenshot:" in flat and reason in flat, f"{name}: the model is told why the screenshot did not come through")
        finally:
            oc.installed_local_tags = real_tags


# ----------------------------------------------------------------------------- 5. attached, to a model that sees


async def test_an_attached_image_goes_to_a_session_model_that_sees() -> None:
    daemon = FakeDaemon(text_reply("a red square"))
    agent_common.build_model = lambda model_id=None: oc.OllamaCloudModel("mistral-large-4", transport=daemon.transport)
    server, conn = fresh_server(ROUTE)
    sid = (await server.new_session(cwd=str(project("vision-native")))).session_id
    agent = server._sessions[sid].agent
    check(acp_server._native_vision(agent) == "mistral-large-4:cloud", "a :cloud vision model sees images itself")
    response = await server.prompt(sid, [acp.text_block("what is this?"), acp.schema.ImageContentBlock(type="image", data=B64, mime_type="image/jpeg")])
    await settle()
    bodies = [json.loads(r.content) for r in daemon.requests]
    with_image = [body for body in bodies if images_in(body)]
    check(response.stop_reason == "end_turn" and with_image, f"the turn ran with the image ({len(bodies)} request(s))")
    check(all(body.get("model") == "mistral-large-4:cloud" for body in with_image), "…on the session's own model, no detour")
    check("no vision model is configured" not in " ".join(str(u) for u in conn.updates), "…and nothing said it was refused")
    held = [b for m in agent.messages for b in m.get("content") or [] if isinstance(b, dict) and "image" in b]
    check(not held, "the image is out of the history afterwards")
    agent_common.build_model = lambda model_id=None: oc.OllamaCloudModel("glm-5.3-flash", transport=daemon.transport)
    check(acp_server._native_vision(server._sessions[(await server.new_session(cwd=str(project("vision-blind")))).session_id].agent) is None, "a text-only model does not")


async def main() -> None:
    test_the_plan()
    await test_a_screenshot_turn_runs_on_the_vision_model_once()
    await test_a_screenshot_that_may_not_go_is_not_sent_and_the_model_is_told()
    await test_an_attached_image_goes_to_a_session_model_that_sees()
    print("\nvision turn: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())
