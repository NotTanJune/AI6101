"""Score validation shared by new judgments and the offline artifact audit."""
METRICS = ("task_fulfillment", "grounding", "planning_and_efficiency")


def valid_scores(result):
    return (isinstance(result, dict) and "error" not in result
            and all(type(result.get(key)) is int and 1 <= result[key] <= 10
                    for key in METRICS))
