"""Manual maintenance: run a full dedup sweep over the real memory
collection without waiting for the model to decide to call
agent_memory(action="curate") itself. Run directly:

    backend/.venv/bin/python _curate_agent_memory.py
"""

from agent_memory_tool import _store

if __name__ == "__main__":
    print(_store.curate())
