from pydoover.docker import run_app

from .application import AssistantGatewayApplication


def main():
    """Run the application."""
    run_app(AssistantGatewayApplication())
