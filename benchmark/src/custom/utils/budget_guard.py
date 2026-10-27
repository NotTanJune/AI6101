"""Conservative per-process reservations for OpenAI-compatible model calls."""
from __future__ import annotations

import json
import threading
import time
import uuid


class BudgetExceededError(RuntimeError):
    """Raised before dispatch when the child cannot reserve the next paid call."""


def _read(obj, name, default=None):
    if isinstance(obj, dict):
        return obj.get(name, default)
    return getattr(obj, name, default)


class BudgetGuard:
    def __init__(self, *, provider, model, remaining_usd, max_output_tokens,
                 rate_card, emit, run_id, strategy):
        self.provider = provider
        self.model = model
        self.remaining_usd = remaining_usd
        self.max_output_tokens = max_output_tokens
        self.rate_card = rate_card or {}
        self.emit = emit
        self.run_id = run_id
        self.strategy = strategy
        self.reserved_usd = 0.0
        self.settled_usd = 0.0
        self.unknown_spend = False
        # The runner may dispatch from several task threads; counters change under this lock.
        self.lock = threading.Lock()
        self.rate = next((entry for entry in self.rate_card.get("entries", [])
                          if entry.get("provider") == provider and entry.get("model") == model), None)
        if provider == "openai" and self.rate is None:
            raise ValueError(f"Rate card has no {provider}/{model} entry")

    def _reserve(self, request):
        if self.unknown_spend:
            raise BudgetExceededError("Previous paid request has unknown usage; stopping to limit exposure")
        if self.provider != "openai":
            return 0.0
        encoded = json.dumps(request, ensure_ascii=False, separators=(",", ":")).encode()
        input_estimate = len(encoded) * 2 + 2048
        input_rate = float(self.rate["input_usd_per_mtok"])
        output_rate = float(self.rate["output_usd_per_mtok"])
        reserve = (input_estimate * input_rate + self.max_output_tokens * output_rate) / 1_000_000
        if self.settled_usd + self.reserved_usd + reserve > self.remaining_usd + 1e-12:
            raise BudgetExceededError(
                f"next call reserve ${reserve:.6f} exceeds remaining child budget "
                f"${max(0.0, self.remaining_usd - self.settled_usd - self.reserved_usd):.6f}"
            )
        self.reserved_usd += reserve
        return reserve

    def dispatch(self, create, request, phase):
        with self.lock:
            reserve = self._reserve(request)
        started = time.perf_counter()
        try:
            response = create(**request)
        except Exception as error:
            # An HTTP error status means the provider answered and rejected the request (for
            # example 429), which is not billed. Without a response (timeout, dropped
            # connection) the request may have been processed, so usage becomes unknown.
            with self.lock:
                if getattr(error, "status_code", None) is not None:
                    self.reserved_usd -= reserve
                elif self.provider == "openai":
                    self.unknown_spend = True  # keep the reservation: spend may have occurred
            self._emit_call(None, None, None, reserve, phase, started)
            raise

        usage = getattr(response, "usage", None)
        input_tokens = _read(usage, "prompt_tokens")
        output_tokens = _read(usage, "completion_tokens")
        if input_tokens is None or output_tokens is None:
            with self.lock:
                self.unknown_spend = self.provider == "openai"
            self._emit_call(None, None, None, reserve, phase, started, response)
        else:
            cost = None
            if self.provider == "openai":
                cached = _read(_read(usage, "prompt_tokens_details", {}), "cached_tokens", 0) or 0
                cost = self._price(int(input_tokens), int(cached), int(output_tokens))
                with self.lock:
                    self.reserved_usd -= reserve
                    self.settled_usd += cost
            self._emit_call(int(input_tokens), int(output_tokens), cost, reserve, phase, started, response)
            with self.lock:
                if self.provider == "openai" and self.settled_usd + self.reserved_usd > self.remaining_usd + 1e-12:
                    self.unknown_spend = True
        return response

    def _price(self, input_tokens, cached_tokens, output_tokens):
        rate = self.rate
        long = (rate.get("long_context_threshold_tokens") is not None
                and input_tokens > rate["long_context_threshold_tokens"])
        input_rate = rate.get("long_input_usd_per_mtok") if long else None
        output_rate = rate.get("long_output_usd_per_mtok") if long else None
        cached_rate = rate.get("long_cached_input_usd_per_mtok") if long else None
        input_rate = rate.get("input_usd_per_mtok") if input_rate is None else input_rate
        output_rate = rate.get("output_usd_per_mtok") if output_rate is None else output_rate
        cached_rate = rate.get("cached_input_usd_per_mtok") if cached_rate is None else cached_rate
        cached_rate = input_rate if cached_rate is None else cached_rate
        return ((max(0, input_tokens - cached_tokens) * input_rate)
                + min(input_tokens, cached_tokens) * cached_rate
                + output_tokens * output_rate) / 1_000_000

    def _emit_call(self, input_tokens, output_tokens, cost, reserve, phase, started, response=None):
        latency = max(0, round((time.perf_counter() - started) * 1000))
        request_id = getattr(response, "id", None) or str(uuid.uuid4())
        local = self.provider == "local"
        self.emit({
            "event_type": "model_call", "provider": self.provider, "model": self.model,
            "phase": phase, "request_id": request_id,
            "input_tokens": input_tokens, "output_tokens": output_tokens,
            "latency_ms": latency,
            "rate_card_id": self.rate_card.get("id") if self.provider == "openai" else None,
            "rate_card_date": self.rate_card.get("date") if self.provider == "openai" else None,
            "estimated_usd": cost,
            "cost_status": "priced" if cost is not None else "local" if local else "unknown",
        })

